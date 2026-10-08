# Statistics

Whole-graph values. Three of them are eager and return a Python number. `describe` is lazy and returns a one-row frame.

## `ur.describe`

```py
ur.describe(edges, full: bool = False) -> NodeFrame
```

A lazy one-row summary with the columns `n_nodes`, `n_edges`, `density`, and `avg_degree`. With `full=True`, the row also has `n_components`, which costs a components pass.

The result is not keyed by node. Call `collect()` on it directly; the shared row methods do not apply.

## `ur.density`

```py
ur.density(edges) -> float
```

The directed edge density: `m / (n · (n − 1))`, the edges present over the edges possible. Self-loops are excluded from the count of possible edges. Parallel edges count as given. For the simple-graph value, build the frame from the distinct rows first; see [Reshaped edge sets](../guides/subgraphs.md#reshaped-edge-sets).

## `ur.avg_path_length`

```py
ur.avg_path_length(edges, sample: float | None = None) -> float
```

The mean shortest-path length over the ordered pairs that have a path, following out-edges. `sample` (a fraction in `(0, 1]`) estimates it from a subset of sources.

## `ur.diameter`

```py
ur.diameter(edges, approximate: bool = True) -> int
```

The longest shortest path over the pairs that have a path, following out-edges. The default is a lower-bound estimate. `approximate=False` gives the exact value, which costs one BFS per node.
