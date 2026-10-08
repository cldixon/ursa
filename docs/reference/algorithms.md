# Algorithms

Each function in this page takes an `EdgeFrame` as its first argument. The node-valued functions return an expression that is also a lazy `NodeFrame` of `(id, value)`. See [Expressions](../concepts/expressions.md#graph-algorithms-are-expressions).

Every kernel is deterministic. The same input and the same parameters give the same output, on any number of threads. The kernels that use randomness take a `seed`.

## Node-valued kernels

### `ur.degree`

```py
ur.degree(edges, direction: str = "out")
```

The number of edges at each node. `direction` is `"out"`, `"in"`, or `"both"`. Output type: `uint32`.

### `ur.pagerank`

```py
ur.pagerank(edges, damping: float = 0.85, max_iter: int = 30, tol: float = 1e-6,
            weight=None, dtype: str = "f64")
```

PageRank over the directed rows. The scores sum to 1. A node with no out-edges spreads its score to all nodes. With `weight=`, each out-edge takes a share of the score in proportion to its weight. Output type: `float64`, or `float32` with `dtype="f32"`.

### `ur.connected_components`

```py
ur.connected_components(edges, mode: str = "weak")
```

One integer label per node. `mode="weak"` ignores direction: two nodes share a label if a path connects them. `mode="strong"` follows direction: two nodes share a label only if each can reach the other. Labels are stable but arbitrary. Group by the label to get the components.

### `ur.triangle_count`

```py
ur.triangle_count(edges)
```

The number of triangles each node is part of, on the undirected view of the rows.

### `ur.clustering_coefficient`

```py
ur.clustering_coefficient(edges, dtype: str = "f64")
```

The local clustering coefficient, on the undirected view. Output type: `float64`, or `float32` with `dtype="f32"`.

### `ur.closeness`

```py
ur.closeness(edges, weight=None, dtype: str = "f64")
```

Closeness centrality, following out-edges. The value is the number of reachable nodes divided by the sum of the distances to them. A node that reaches nothing scores `0.0`. With `weight=`, the distances are weighted path costs.

### `ur.betweenness`

```py
ur.betweenness(edges, sample: float | None = None, weight=None, seed: int | None = None,
               dtype: str = "f64")
```

Betweenness centrality (Brandes), directed and not normalized. The exact computation is O(n·m). `sample=` (a fraction in `(0, 1]`) estimates it from a random subset of source nodes, scaled to the full count. `seed` makes the estimate reproducible. With `weight=`, the shortest paths are weighted.

### `ur.label_propagation`

```py
ur.label_propagation(edges, max_iter: int = 20, seed: int | None = None)
```

Community detection by label propagation, on the undirected view. One integer label per node. The result is a heuristic: compare the modularity, not the labels.

### `ur.louvain`

```py
ur.louvain(edges, weight=None, resolution: float = 1.0, seed: int | None = None)
```

Community detection by Louvain modularity optimization, on the undirected view. One integer label per node. A larger `resolution` gives more, smaller communities. With `weight=`, the modularity is weighted.

## Neighbour aggregation

### `ur.neighbors`

```py
ur.neighbors(edges, direction: str = "out", from_=None).agg(expr, dtype: str = "f64")
```

For each node, an aggregation of an attribute over its neighbours. `expr` is an [aggregation expression](expressions.md#aggregations), for example `ur.col("age").mean()`. The topology comes from `edges`. The attribute comes from the `NodeFrame` the expression runs in, or from `from_` if given.

`mean`, `sum`, `min`, and `max` need a numeric attribute. `count` and `n_unique` accept a string attribute too.

## Traversals

Traversals return an `EdgeFrame` or a `NodeFrame` with rows of their own. The shared methods (`filter`, `sort`, `head`, ...) apply to the result, and a node-valued kernel can run over it.

### `ur.hop`

```py
ur.hop(edges, n: int = 1, direction: str = "out").from_(seeds) -> EdgeFrame
```

The nodes within `n` hops of each seed. `seeds` is a list of ids. The result has one row per `(seed, reached)` pair, in the columns `src` and `dst`. `direction` is `"out"`, `"in"`, or `"both"`.

### `ur.shortest_path`

```py
ur.shortest_path(edges, source, target, weight=None, direction: str = "out") -> EdgeFrame
```

One shortest path from `source` to `target`. The result has one row per edge on the path, in order, with the columns `src`, `dst`, `hop`, and `cost`. `hop` is the 0-based position. `cost` is the total cost from `source` to that edge's `dst`. Without `weight=`, the path has the fewest edges and `cost` is `hop + 1`. With `weight=`, the path has the least total weight (Dijkstra), and weights must not be negative. If there is no path, the result is empty.

### `ur.random_walk`

```py
ur.random_walk(edges, start, steps: int, walks_per_node: int = 1, seed: int | None = None) -> NodeFrame
```

Random walks along out-edges. `start` is a list of ids. The result has the columns `walk_id`, `step`, and `node`, one row per position on each walk. A walk stops early at a node with no out-edges. `seed` makes the walks reproducible.

## The `weight` parameter

`weight` is an expression over the columns of `edges`, for example `ur.col("amount")` or `ur.col("amount") * ur.col("fx")`. It gives one `float64` per edge. A null weight is an error. See [Weight the edges](../guides/weights.md).

## The `dtype` parameter

`dtype="f32"` emits a float-valued column as `float32`. The kernel still computes in `float64` and casts on output. The default is `"f64"`.
