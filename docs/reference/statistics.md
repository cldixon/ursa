# Statistics

Whole-graph values. Each is a lazy one-row frame. `describe` always returns the frame. `density`, `avg_path_length`, and `diameter` return a Python number by default: they collect the frame for you. Pass `lazy=True` to get the frame instead, which flows anywhere a frame flows.

Each reads the same subgraph view as `degree` and the other graph ops. Over a filtered edge frame, that is the parent graph's nodes and only the rows the filter keeps. Over a `hop` or `shortest_path` result, it is the subgraph the reached nodes induce. After a reshaping step (`distinct`, `head`, a join, ...) a stat reads the graph of the reshaped rows, as a new frame over them would.

Each takes `direction`: `"out"` (the default), `"in"`, or `"both"`. The default reads the rows as directed. `"both"` reads them as the simple undirected graph: one edge for each distinct pair of nodes joined by a row in either direction, with self-loops dropped. That is the value NetworkX gives for `G.to_undirected()`. `"in"` follows edges backward, and over the whole graph it gives the same value as `"out"`. See [Semantics vs NetworkX](../semantics.md#undirected-graphs).

## `ur.describe`

```py
ur.describe(edges, full: bool = False, direction: str = "out", nodes: str = "all") -> NodeFrame
```

A lazy one-row summary with the columns `n_nodes`, `n_edges`, `density`, and `avg_degree`. With `full=True`, the row also has `n_components`, which costs a components pass. `n_components` counts weak components in every direction.

By default, `n_edges` is the number of rows and `avg_degree` is the mean out-degree. With `direction="both"`, `n_edges` is the number of undirected edges, `density` is the undirected density, and `avg_degree` is the mean undirected degree, `2 · n_edges / n_nodes`.

Over a filtered edge frame or a traversal result, `n_nodes` is still the parent graph's node count by default. Every node the filter or traversal leaves without an edge counts as its own component, and `density` divides by the parent's `n`, so a small neighbourhood of a large graph has a density near 0.

`nodes="touched"` describes only the nodes the view touches: the nodes at an end of a kept edge. `n_nodes` counts them, `density` and `avg_degree` divide by that count, and `n_components` leaves the other nodes out. These are the figures a new frame over the kept rows gives. Over a whole graph every node is at an end of an edge, so `nodes="touched"` changes nothing there. Any other value raises `ValueError`.

The result is not keyed by node. The relational methods (`filter`, `with_columns`, `select`, `join`, ...) run on its one row.

## `ur.density`

```py
ur.density(edges, direction: str = "out", *, lazy: bool = False) -> float | NodeFrame
```

With `lazy=True`, a one-row frame with one `density` column (`float64`, not null). Otherwise the value as a `float`.

The directed edge density by default: `m / (n · (n − 1))`, the edges present over the edges possible. Self-loops are excluded from the count of possible edges. Parallel edges count as given. For the simple-graph value, build the frame from the distinct rows first; see [Reshaped edge sets](../guides/subgraphs.md#reshaped-edge-sets).

With `direction="both"`, the undirected density: `m_u / (n · (n − 1) / 2)`, where `m_u` is the number of distinct pairs joined by a row in either direction. A reciprocal pair and parallel rows count once, and self-loops are not counted. The value is always between 0 and 1, and it equals `nx.density(G.to_undirected())` for a graph without self-loops.

Over a filtered edge frame or a traversal result, the edges are the kept ones and `n` is the parent's node count: the `density` that `describe` reports for the same frame.

## `ur.avg_path_length`

```py
ur.avg_path_length(edges, sample: float | None = None, direction: str = "out", *,
                   lazy: bool = False) -> float | NodeFrame
```

The mean shortest-path length over the ordered pairs that have a path, following out-edges. With `direction="both"`, paths ignore direction, which gives `nx.average_shortest_path_length(G.to_undirected())` on a connected graph. `sample` (a fraction in `(0, 1]`) estimates it from a subset of sources. With `lazy=True`, a one-row frame with one `avg_path_length` column (`float64`, not null).

Over a filtered edge frame or a traversal result, paths follow only the kept edges, and `sample` picks its sources among the nodes at an end of a kept edge. The exact value equals the value over a new frame of the kept rows.

## `ur.diameter`

```py
ur.diameter(edges, approximate: bool = True, direction: str = "out", *,
            lazy: bool = False) -> int | NodeFrame
```

The longest shortest path over the pairs that have a path, following out-edges. With `direction="both"`, paths ignore direction, which gives `nx.diameter(G.to_undirected())` on a connected graph. The default is a lower-bound estimate. `approximate=False` gives the exact value, which costs one BFS per node. With `lazy=True`, a one-row frame with one `diameter` column (`int64`, not null).

Over a filtered edge frame or a traversal result, paths follow only the kept edges, and the estimate picks its sources among the nodes at an end of a kept edge. The exact value equals the value over a new frame of the kept rows.
