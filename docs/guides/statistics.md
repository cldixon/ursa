# Summarize the graph

Four functions describe the whole graph. `describe` is lazy and returns a one-row frame. `density`, `avg_path_length`, and `diameter` return a number, or with `lazy=True` a one-row frame like `describe`.

```python
import ursa as ur

edges = ur.datasets.load_karate()
```

## `describe`

```python
print(ur.describe(edges).collect())
print(ur.describe(edges, full=True).collect())
```

```text
shape: (1, 5)
┌─────────┬─────────┬───────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density   ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---       ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double    ┆ double     ┆ int64        │
╞═════════╪═════════╪═══════════╪════════════╪══════════════╡
│ 34      ┆ 78      ┆ 0.0695187 ┆ 2.29412    ┆ null         │
└─────────┴─────────┴───────────┴────────────┴──────────────┘
shape: (1, 5)
┌─────────┬─────────┬───────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density   ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---       ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double    ┆ double     ┆ int64        │
╞═════════╪═════════╪═══════════╪════════════╪══════════════╡
│ 34      ┆ 78      ┆ 0.0695187 ┆ 2.29412    ┆ 1            │
└─────────┴─────────┴───────────┴────────────┴──────────────┘
```

`full=True` adds `n_components`, which costs a components pass. The row methods run on the summary's one row.

## Density

The directed edge density: the edges present over the edges possible, `m / (n · (n − 1))`.

```python
print(ur.density(edges))
```

```text
0.06951871657754011
```

Parallel edges count as given. For the simple-graph value, build the frame from the distinct rows first; see [Reshaped edge sets](subgraphs.md#reshaped-edge-sets).

With `lazy=True`, the stat is a one-row frame, which composes with other frames before anything runs:

```python
print(ur.density(edges, lazy=True).select("density", (ur.col("density") * 100).alias("pct")).collect())
```

```text
shape: (1, 2)
┌───────────┬─────────┐
│ density   ┆ pct     │
│ ---       ┆ ---     │
│ double    ┆ double  │
╞═══════════╪═════════╡
│ 0.0695187 ┆ 6.95187 │
└───────────┴─────────┘
```

## Path statistics

`avg_path_length` and `diameter` follow out-edges. The bundled graphs store each undirected edge once, so add the reverse rows first to get the undirected values:

```python
import pyarrow as pa

t = edges.collect().to_arrow()
both = ur.EdgeFrame(
    pa.concat_tables([t, t.select(["dst", "src"]).rename_columns(["src", "dst"])]),
    src="src",
    dst="dst",
)

print("average path length:", ur.avg_path_length(both))
print("diameter (exact):   ", ur.diameter(both, approximate=False))
```

```text
average path length: 2.408199643493761
diameter (exact):    5
```

`diameter` is a lower-bound estimate by default. `approximate=False` gives the exact value, at the cost of one search per node. `avg_path_length(edges, sample=0.1)` estimates the mean from a tenth of the sources.

## Over a subgraph

Over a filtered edge frame, every statistic reads the subgraph view: the parent graph's nodes, and only the rows the filter keeps. `describe`, `density`, `avg_path_length`, and `diameter` agree with each other and with `degree` on the same frame. A `hop` or `shortest_path` result is the subgraph its reached nodes induce.

```python
club = edges.filter((ur.col("src") < 10) & (ur.col("dst") < 10))

print(ur.describe(club).collect())
print(ur.density(club), ur.diameter(club, approximate=False, direction="both"))
```

```text
shape: (1, 5)
┌─────────┬─────────┬───────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density   ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---       ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double    ┆ double     ┆ int64        │
╞═════════╪═════════╪═══════════╪════════════╪══════════════╡
│ 34      ┆ 18      ┆ 0.0160428 ┆ 0.529412   ┆ null         │
└─────────┴─────────┴───────────┴────────────┴──────────────┘
0.016042780748663103 3
```

`n_nodes` is the parent's 34, so the density of a small neighbourhood of a large graph is near 0. `nodes="touched"` describes only the nodes at an end of a kept edge, the figures a new frame over the kept rows gives:

```python
print(ur.describe(club, full=True, nodes="touched").collect())
```

```text
shape: (1, 5)
┌─────────┬─────────┬─────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---     ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double  ┆ double     ┆ int64        │
╞═════════╪═════════╪═════════╪════════════╪══════════════╡
│ 10      ┆ 18      ┆ 0.2     ┆ 1.8        ┆ 1            │
└─────────┴─────────┴─────────┴────────────┴──────────────┘
```
