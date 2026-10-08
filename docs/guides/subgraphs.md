# Work on a subgraph

An `EdgeFrame` with fewer rows is a smaller graph. Any algorithm over it sees only those rows. You get a smaller graph in three ways: a filter on the edges, a traversal, or `distinct`.

```python
import ursa as ur

edges = ur.datasets.load_lesmis()
```

## Filter the edges

A `filter` on an `EdgeFrame` is a subgraph. Run an algorithm over the filtered frame, not over the original:

```python
strong = edges.filter(ur.col("weight") >= 5)
print(ur.describe(strong).collect())

core = strong.nodes().with_columns(
    degree=ur.degree(strong, direction="both"),
    component=ur.connected_components(strong),
)
print(core.sort("degree", descending=True).head(5).collect())
```

```text
shape: (1, 5)
┌─────────┬─────────┬──────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density  ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---      ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double   ┆ double     ┆ int64        │
╞═════════╪═════════╪══════════╪════════════╪══════════════╡
│ 77      ┆ 254     ┆ 0.043404 ┆ 3.2987     ┆ null         │
└─────────┴─────────┴──────────┴────────────┴──────────────┘
shape: (5, 3)
┌────────────┬────────┬───────────┐
│ id         ┆ degree ┆ component │
│ ---        ┆ ---    ┆ ---       │
│ string     ┆ uint32 ┆ uint32    │
╞════════════╪════════╪═══════════╡
│ Marius     ┆ 9      ┆ 34        │
│ Courfeyrac ┆ 8      ┆ 34        │
│ Enjolras   ┆ 8      ┆ 34        │
│ Valjean    ┆ 8      ┆ 34        │
│ Combeferre ┆ 8      ┆ 34        │
└────────────┴────────┴───────────┘
```

The filtered frame builds its own index on first use. The original frame keeps its index.

## A traversal result

`hop` and `shortest_path` return an `EdgeFrame`. A kernel over it runs on the nodes the traversal reached, with the edges among them.

```python
around_valjean = ur.hop(edges, n=1, direction="both").from_(["Valjean"])

local = ur.pagerank(around_valjean).sort("pagerank", descending=True).head(5)
print(local.collect())
```

```text
shape: (5, 2)
┌──────────────┬───────────┐
│ id           ┆ pagerank  │
│ ---          ┆ ---       │
│ string       ┆ double    │
╞══════════════╪═══════════╡
│ Montparnasse ┆ 0.058014  │
│ Cochepaille  ┆ 0.0471909 │
│ Claquesous   ┆ 0.0373019 │
│ Bossuet      ┆ 0.0355485 │
│ Marius       ┆ 0.0321466 │
└──────────────┴───────────┘
```

## Reshaped edge sets

`distinct`, `sample`, `join`, and `group_by` reshape the edge set in a way the engine cannot express as a mask over the original graph. A graph algorithm over one of those frames raises `NotImplementedError`. Collect the rows and build a new frame from them:

```python
multi = ur.from_edgelist([(0, 1), (0, 1), (1, 2)])
simple = ur.from_arrow(multi.distinct().collect().to_arrow(), src="src", dst="dst")

print(ur.degree(multi).collect().to_dicts())
print(ur.degree(simple).collect().to_dicts())
```

```text
[{'id': 0, 'degree': 2}, {'id': 1, 'degree': 1}, {'id': 2, 'degree': 0}]
[{'id': 0, 'degree': 1}, {'id': 1, 'degree': 1}, {'id': 2, 'degree': 0}]
```

The same two lines turn a random `sample(n, seed=)` of the edges into a graph.
