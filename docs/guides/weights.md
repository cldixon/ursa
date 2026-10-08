# Weight the edges

Nothing in Ursa is weighted unless you ask. `weight=` is an expression over the columns of the `EdgeFrame`. It gives one number per edge. `pagerank`, `shortest_path`, `closeness`, `betweenness`, and `louvain` accept it.

```python
import ursa as ur

edges = ur.datasets.load_lesmis()   # 77 characters; `weight` is the number of shared chapters
print(edges.sort("weight", descending=True).head(3).collect())
```

```text
shape: (3, 3)
┌─────────┬─────────┬────────┐
│ src     ┆ dst     ┆ weight │
│ ---     ┆ ---     ┆ ---    │
│ string  ┆ string  ┆ int64  │
╞═════════╪═════════╪════════╡
│ Valjean ┆ Cosette ┆ 31     │
│ Cosette ┆ Marius  ┆ 21     │
│ Valjean ┆ Marius  ┆ 19     │
└─────────┴─────────┴────────┘
```

## A weighted kernel

Pass the column as an expression:

```python
scores = edges.nodes().with_columns(
    plain=ur.pagerank(edges),
    weighted=ur.pagerank(edges, weight=ur.col("weight")),
)
print(scores.sort("weighted", descending=True).head(5).collect())
```

```text
shape: (5, 3)
┌──────────────┬───────────┬───────────┐
│ id           ┆ plain     ┆ weighted  │
│ ---          ┆ ---       ┆ ---       │
│ string       ┆ double    ┆ double    │
╞══════════════╪═══════════╪═══════════╡
│ MmeHucheloup ┆ 0.0648352 ┆ 0.0637729 │
│ Joly         ┆ 0.0288011 ┆ 0.0468511 │
│ Grantaire    ┆ 0.0376531 ┆ 0.0463043 │
│ Fantine      ┆ 0.0412951 ┆ 0.038204  │
│ Bossuet      ┆ 0.0228116 ┆ 0.0377904 │
└──────────────┴───────────┴───────────┘
```

## A derived weight

The expression can combine columns and constants. The engine evaluates it per edge before the kernel runs. Here a strong co-appearance becomes a short distance, so the shortest path prefers characters who share many chapters:

```python
path = ur.shortest_path(
    edges, "Valjean", "Gavroche",
    weight=1.0 / ur.col("weight"),
    direction="both",
)
print(path.collect())
```

```text
shape: (2, 4)
┌─────────┬──────────┬───────┬───────────┐
│ src     ┆ dst      ┆ hop   ┆ cost      │
│ ---     ┆ ---      ┆ ---   ┆ ---       │
│ string  ┆ string   ┆ int64 ┆ double    │
╞═════════╪══════════╪═══════╪═══════════╡
│ Valjean ┆ Marius   ┆ 0     ┆ 0.0526316 │
│ Marius  ┆ Gavroche ┆ 1     ┆ 0.302632  │
└─────────┴──────────┴───────┴───────────┘
```

`cost` is the sum of the weights along the path.

!!! note "Integer columns"

    `weight` in this dataset is an integer column. `1 / ur.col("weight")` is an integer division and gives `0` for every edge. Write `1.0 / ur.col("weight")` to get a float.

## Weighted communities

```python
communities = edges.nodes().with_columns(community=ur.louvain(edges, weight=ur.col("weight"), seed=1))
sizes = communities.group_by("community").agg(size=ur.col("id").count()).sort("size", descending=True)
print(sizes.collect())
```

```text
shape: (6, 2)
┌───────────┬───────┐
│ community ┆ size  │
│ ---       ┆ ---   │
│ uint32    ┆ int64 │
╞═══════════╪═══════╡
│ 1         ┆ 23    │
│ 5         ┆ 17    │
│ 2         ┆ 11    │
│ 3         ┆ 10    │
│ 0         ┆ 10    │
│ 4         ┆ 6     │
└───────────┴───────┘
```

## Rules

- A weight must be numeric. The engine casts it to `float64`.
- A null weight is an error. Fill or drop the rows first.
- `shortest_path` rejects a negative weight.
- The expression resolves against the edge columns. `ur.col("weight")` in `weight=` is the edge column, not a node attribute.
