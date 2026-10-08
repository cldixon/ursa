# Compute graph metrics

A graph algorithm in Ursa is a function of an `EdgeFrame`. It returns one value per node. You use it as a column, or as a table on its own.

```python
import ursa as ur

edges = ur.datasets.load_karate()
```

## As columns

Put every metric you need in one `with_columns` call. The engine builds the graph index once and runs the kernels over it.

```python
metrics = edges.nodes().with_columns(
    degree=ur.degree(edges, direction="both"),
    pagerank=ur.pagerank(edges, damping=0.85),
    triangles=ur.triangle_count(edges),
    clustering=ur.clustering_coefficient(edges),
    component=ur.connected_components(edges),
    community=ur.louvain(edges, seed=1),
)
print(metrics.sort("id").head(5).collect())
```

```text
shape: (5, 7)
┌───────┬────────┬───────────┬───────────┬────────────┬───────────┬───────────┐
│ id    ┆ degree ┆ pagerank  ┆ triangles ┆ clustering ┆ component ┆ community │
│ ---   ┆ ---    ┆ ---       ┆ ---       ┆ ---        ┆ ---       ┆ ---       │
│ int64 ┆ uint32 ┆ double    ┆ uint32    ┆ double     ┆ uint32    ┆ uint32    │
╞═══════╪════════╪═══════════╪═══════════╪════════════╪═══════════╪═══════════╡
│ 0     ┆ 16     ┆ 0.0150605 ┆ 18        ┆ 0.15       ┆ 29        ┆ 0         │
│ 1     ┆ 9      ┆ 0.0158606 ┆ 12        ┆ 0.333333   ┆ 29        ┆ 0         │
│ 2     ┆ 10     ┆ 0.0175458 ┆ 11        ┆ 0.244444   ┆ 29        ┆ 0         │
│ 3     ┆ 6      ┆ 0.01941   ┆ 10        ┆ 0.666667   ┆ 29        ┆ 0         │
│ 4     ┆ 3      ┆ 0.0158606 ┆ 2         ┆ 0.666667   ┆ 29        ┆ 1         │
└───────┴────────┴───────────┴───────────┴────────────┴───────────┴───────────┘
```

## As a table

Called on its own, an algorithm is a lazy table of `(id, value)`. The row methods apply to it.

```python
print(ur.betweenness(edges).sort("betweenness", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬─────────────┐
│ id    ┆ betweenness │
│ ---   ┆ ---         │
│ int64 ┆ double      │
╞═══════╪═════════════╡
│ 2     ┆ 8.83333     │
│ 31    ┆ 5.08333     │
│ 8     ┆ 2.25        │
└───────┴─────────────┘
```

## The kernels

| Function | Value per node | Reads the rows as |
|---|---|---|
| `ur.degree(edges, direction="out")` | the number of edges, `uint32` | per `direction` |
| `ur.pagerank(edges, damping=0.85)` | the PageRank score; the scores sum to 1 | directed |
| `ur.closeness(edges)` | reachable nodes over the sum of distances | directed |
| `ur.betweenness(edges)` | the number of shortest paths through the node | directed |
| `ur.triangle_count(edges)` | the number of triangles the node is in | undirected |
| `ur.clustering_coefficient(edges)` | closed triples over open triples | undirected |
| `ur.connected_components(edges)` | a component label | undirected (`mode="weak"`) |
| `ur.connected_components(edges, mode="strong")` | a component label | directed |
| `ur.louvain(edges, seed=...)` | a community label | undirected |
| `ur.label_propagation(edges, seed=...)` | a community label | undirected |

The full parameter list is in the [reference](../reference/algorithms.md).

## Labels

`connected_components`, `louvain`, and `label_propagation` give an integer label per node. The label values are arbitrary. Group by the label to get the members:

```python
communities = (
    edges.nodes()
    .with_columns(community=ur.louvain(edges, seed=1))
    .group_by("community")
    .agg(size=ur.col("id").count())
    .sort("size", descending=True)
)
print(communities.collect())
```

```text
shape: (4, 2)
┌───────────┬───────┐
│ community ┆ size  │
│ ---       ┆ ---   │
│ uint32    ┆ int64 │
╞═══════════╪═══════╡
│ 2         ┆ 13    │
│ 0         ┆ 12    │
│ 1         ┆ 5     │
│ 3         ┆ 4     │
└───────────┴───────┘
```

## Approximate betweenness

Exact betweenness runs one shortest-path pass per node. On a large graph, `sample=` runs it from a random fraction of the nodes and scales the result. `seed=` makes the estimate reproducible.

```python
estimate = ur.betweenness(edges, sample=0.5, seed=1)
print(estimate.sort("betweenness", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬─────────────┐
│ id    ┆ betweenness │
│ ---   ┆ ---         │
│ int64 ┆ double      │
╞═══════╪═════════════╡
│ 2     ┆ 6.66667     │
│ 31    ┆ 6.16667     │
│ 6     ┆ 3           │
└───────┴─────────────┘
```

## Smaller output

The float-valued kernels accept `dtype="f32"`. The kernel still computes in `float64` and casts on output. Use it when a column of positions or scores goes to disk.

```python
small = ur.pagerank(edges, dtype="f32").head(2).collect()
print(small)
```

```text
shape: (2, 2)
┌───────┬───────────┐
│ id    ┆ pagerank  │
│ ---   ┆ ---       │
│ int64 ┆ float     │
╞═══════╪═══════════╡
│ 0     ┆ 0.0150605 │
│ 1     ┆ 0.0158606 │
└───────┴───────────┘
```

## Determinism

Every kernel gives the same output on every run and on every thread count. The random kernels (`louvain`, `label_propagation`, sampled `betweenness`, `random_walk`) are reproducible from `seed=`. Without a seed, each call picks its own.
