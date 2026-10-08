# Summarize the graph

Four functions describe the whole graph. `describe` is lazy and returns a one-row frame. `density`, `avg_path_length`, and `diameter` are eager and return a number.

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

`full=True` adds `n_components`, which costs a components pass. Collect `describe` on its own; the row methods do not apply to it.

## Density

The directed edge density: the edges present over the edges possible, `m / (n · (n − 1))`.

```python
print(ur.density(edges))
```

```text
0.06951871657754011
```

Parallel edges count as given. For the simple-graph value, build the frame from the distinct rows first; see [Reshaped edge sets](subgraphs.md#reshaped-edge-sets).

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
