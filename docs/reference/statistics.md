# Statistics

Whole-graph values. Three of them are eager and return a Python number. `describe` is lazy and returns a one-row frame.

Each takes `direction`: `"out"` (the default), `"in"`, or `"both"`. The default reads the rows as directed. `"both"` reads them as the simple undirected graph: one edge for each distinct pair of nodes joined by a row in either direction, with self-loops dropped. That is the value NetworkX gives for `G.to_undirected()`. `"in"` follows edges backward, and over the whole graph it gives the same value as `"out"`. See [Semantics vs NetworkX](../semantics.md#undirected-graphs).

## `ur.describe`

```py
ur.describe(edges, full: bool = False, direction: str = "out") -> NodeFrame
```

A lazy one-row summary with the columns `n_nodes`, `n_edges`, `density`, and `avg_degree`. With `full=True`, the row also has `n_components`, which costs a components pass. `n_components` counts weak components in every direction.

By default, `n_edges` is the number of rows and `avg_degree` is the mean out-degree. With `direction="both"`, `n_edges` is the number of undirected edges, `density` is the undirected density, and `avg_degree` is the mean undirected degree, `2 · n_edges / n_nodes`.

Over a filtered edge frame, `describe` reads the same subgraph view as `degree` and the other graph ops: the parent graph's nodes, and only the rows the filter keeps. The same holds for a `hop` or `shortest_path` result, whose edges are the subgraph its reached nodes induce. `n_nodes` is still the parent graph's node count. Every node the filter or traversal leaves without an edge counts as its own component, and `density` divides by the parent's `n`, so a small neighbourhood of a large graph has a density near 0. For figures over only the kept rows' nodes, rebuild a frame from the collected rows. After a reshaping step (`distinct`, `head`, a join, ...) `describe` reads the graph of the reshaped rows, as a new frame over them would.

The result is not keyed by node. Call `collect()` on it directly; the shared row methods do not apply.

## `ur.density`

```py
ur.density(edges, direction: str = "out") -> float
```

The directed edge density by default: `m / (n · (n − 1))`, the edges present over the edges possible. Self-loops are excluded from the count of possible edges. Parallel edges count as given. For the simple-graph value, build the frame from the distinct rows first; see [Reshaped edge sets](../guides/subgraphs.md#reshaped-edge-sets).

With `direction="both"`, the undirected density: `m_u / (n · (n − 1) / 2)`, where `m_u` is the number of distinct pairs joined by a row in either direction. A reciprocal pair and parallel rows count once, and self-loops are not counted. The value is always between 0 and 1, and it equals `nx.density(G.to_undirected())` for a graph without self-loops.

## `ur.avg_path_length`

```py
ur.avg_path_length(edges, sample: float | None = None, direction: str = "out") -> float
```

The mean shortest-path length over the ordered pairs that have a path, following out-edges. With `direction="both"`, paths ignore direction, which gives `nx.average_shortest_path_length(G.to_undirected())` on a connected graph. `sample` (a fraction in `(0, 1]`) estimates it from a subset of sources.

## `ur.diameter`

```py
ur.diameter(edges, approximate: bool = True, direction: str = "out") -> int
```

The longest shortest path over the pairs that have a path, following out-edges. With `direction="both"`, paths ignore direction, which gives `nx.diameter(G.to_undirected())` on a connected graph. The default is a lower-bound estimate. `approximate=False` gives the exact value, which costs one BFS per node.
