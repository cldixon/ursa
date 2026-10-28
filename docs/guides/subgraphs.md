# Work on a subgraph

An `EdgeFrame` with fewer rows is a smaller graph. Any algorithm over it sees only those rows. You get a smaller graph in three ways: a filter on the edges, a traversal, or a step that reshapes the edge rows, such as `distinct`.

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

A traversal over a filtered frame, such as `ur.hop(strong, n=2)`, follows only the edges the filter keeps. A kernel over its result stays inside the filtered graph too. `random_walk` over a filtered frame also takes only the kept edges.

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

`distinct`, `sample`, `join`, `group_by().agg()`, `sort` and `head` change the edge rows themselves. An algorithm or traversal over one of those frames runs over the graph of its rows: the same graph you get by collecting the rows and building a new frame from them, with the same node order.

```python
multi = ur.from_edgelist([(0, 1), (0, 1), (1, 2)])

print(ur.degree(multi).collect().to_dicts())
print(ur.degree(multi.distinct()).collect().to_dicts())
```

```text
[{'id': 0, 'degree': 2}, {'id': 1, 'degree': 1}, {'id': 2, 'degree': 0}]
[{'id': 0, 'degree': 1}, {'id': 1, 'degree': 1}, {'id': 2, 'degree': 0}]
```

A weight can read a column the reshaping step computes, such as an aggregate:

```python
grouped = multi.with_columns(w=ur.lit(1.0)).group_by("src", "dst").agg(w=ur.col("w").sum())
print(ur.pagerank(grouped, weight=ur.col("w")).collect().to_dicts())
```

```text
[{'id': 0, 'pagerank': 0.18441687554671388}, {'id': 1, 'pagerank': 0.34117100648207455}, {'id': 2, 'pagerank': 0.47441211797121174}]
```

The edges must keep their `src` and `dst` columns. A `select` or `group_by` that drops one raises `ColumnNotFoundError`, naming the step. A `rename` does not move a role: the frame's `src` and `dst` columns keep their names, so renaming one away raises the same error. The endpoints are read by name, so a step that writes new values into a `src` or `dst` column (a `rename` onto it, or a `with_columns` or `select` expression named like it) makes a new graph from those values, as a reshaping step does.

Where a `filter` sits decides which nodes stay:

- A `filter` written after the reshaping step is a subgraph view of the reshaped graph, as a filter on any edge frame is. Every node of the reshaped graph stays, isolated ones included.
- A `filter` written before it is part of the reshaped edge set. In `e.filter(p).sort("w")`, the graph is built from the rows the filter keeps, so a node whose every edge was filtered out is not in it.

`select`, `rename` and `with_columns` that leave the endpoints alone keep the rows, so they keep the graph; a filter or a weight after them reads the columns they leave.

The graph of a reshaped frame is built on first use and cached for the session under a fingerprint of the edge plan and its inputs: the in-memory table, or a local file's size and modification time. Collecting the same reshaped frame again reuses it. A frame over a remote file (`s3://`, `https://`, ...) builds it on every collect. The cache holds the four most recently used reshaped graphs, each a whole graph index with the weights and filter masks computed over it, so it can hold as much memory as four indexes of the frame's size. It is freed when the process exits; there is no call to clear it.

## Several edge frames in one query

Each graph column in a `with_columns` names its own edges, so one call can mix them:

```python
nodes = edges.nodes().with_columns(
    degree=ur.degree(edges),
    strong_degree=ur.degree(strong),
)
```

The columns attach to the frame's rows by node id. On an attribute frame (`ur.from_arrow(..., id=)`), its rows decide which nodes appear. On `edges.nodes()`, the first graph column's edges give the node set, or the edges `nodes()` was called on when a reshaping step shaped them. A node that another column's graph does not contain gets a null in that column.
