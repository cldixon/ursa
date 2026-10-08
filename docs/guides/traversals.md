# Traverse the graph

A traversal returns rows of its own: pairs, path edges, or walk steps. The result is a frame, so you can filter it, sort it, write it, or run an algorithm over it.

```python
import ursa as ur

edges = ur.datasets.load_karate()
```

## Hops

`ur.hop(edges, n).from_(seeds)` gives every node within `n` edges of each seed. The result is an `EdgeFrame` with one row per `(seed, reached)` pair, in the columns `src` and `dst`.

```python
within_two = ur.hop(edges, n=2, direction="both").from_([0, 33])
print(within_two.sort("dst").head(5).collect())

from_zero = within_two.filter(ur.col("src") == 0).collect()
print(len(from_zero.to_dicts()), "nodes within two hops of 0")
```

```text
shape: (5, 2)
┌───────┬───────┐
│ src   ┆ dst   │
│ ---   ┆ ---   │
│ int64 ┆ int64 │
╞═══════╪═══════╡
│ 33    ┆ 0     │
│ 0     ┆ 1     │
│ 33    ┆ 1     │
│ 33    ┆ 2     │
│ 0     ┆ 2     │
└───────┴───────┘
25 nodes within two hops of 0
```

`direction` is `"out"` (the default), `"in"`, or `"both"`.

## Shortest paths

`ur.shortest_path(edges, source, target)` gives one shortest path. The result has one row per edge on the path, in order. `hop` is the position, and `cost` is the total cost from `source` to the end of that edge.

```python
path = ur.shortest_path(edges, 0, 26, direction="both")
print(path.collect())
```

```text
shape: (3, 4)
┌───────┬───────┬───────┬────────┐
│ src   ┆ dst   ┆ hop   ┆ cost   │
│ ---   ┆ ---   ┆ ---   ┆ ---    │
│ int64 ┆ int64 ┆ int64 ┆ double │
╞═══════╪═══════╪═══════╪════════╡
│ 0     ┆ 8     ┆ 0     ┆ 1      │
│ 8     ┆ 33    ┆ 1     ┆ 2      │
│ 33    ┆ 26    ┆ 2     ┆ 3      │
└───────┴───────┴───────┴────────┘
```

Without `weight=`, the path has the fewest edges, and `cost` counts them. With `weight=`, the path has the least total weight. See [Weight the edges](weights.md). If no path exists, the result has no rows.

## Random walks

`ur.random_walk` starts walks at the given nodes and follows out-edges at random. The result is a table of `(walk_id, step, node)`, which feeds an embedding pipeline directly.

```python
walks = ur.random_walk(edges, start=[0, 1], steps=4, walks_per_node=2, seed=7)
print(walks.collect())
```

```text
shape: (15, 3)
┌─────────┬───────┬───────┐
│ walk_id ┆ step  ┆ node  │
│ ---     ┆ ---   ┆ ---   │
│ int64   ┆ int64 ┆ int64 │
╞═════════╪═══════╪═══════╡
│ 0       ┆ 0     ┆ 0     │
│ 0       ┆ 1     ┆ 13    │
│ 0       ┆ 2     ┆ 33    │
│ 1       ┆ 0     ┆ 0     │
│ 1       ┆ 1     ┆ 2     │
│ 1       ┆ 2     ┆ 9     │
│ 1       ┆ 3     ┆ 33    │
│ 2       ┆ 0     ┆ 1     │
│ 2       ┆ 1     ┆ 13    │
│ 2       ┆ 2     ┆ 33    │
│ …       ┆ …     ┆ …     │
└─────────┴───────┴───────┘
```

A walk stops early at a node with no out-edges. `seed=` makes the walks reproducible.

## Algorithms over a traversal

A traversal result is an `EdgeFrame`, so a node-valued kernel can run over it. The kernel sees only the nodes the traversal reached. See [Work on a subgraph](subgraphs.md).

```python
region = ur.hop(edges, n=1, direction="both").from_([0])
print(ur.degree(region, direction="both").sort("degree", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬────────┐
│ id    ┆ degree │
│ ---   ┆ ---    │
│ int64 ┆ uint32 │
╞═══════╪════════╡
│ 0     ┆ 16     │
│ 1     ┆ 8      │
│ 2     ┆ 6      │
└───────┴────────┘
```
