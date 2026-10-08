# Direction

Ursa has no `Graph` and `DiGraph` split. Every edge row is directed, from `src` to `dst`. Direction is a parameter of each operation.

## The `direction` parameter

Operations that walk edges accept `direction`:

| Value | Follows |
|---|---|
| `"out"` (default) | edges from the node: `src → dst` |
| `"in"` | edges into the node: `dst → src` |
| `"both"` | edges in either direction |

`degree`, `neighbors`, `hop`, and `shortest_path` accept `direction`.

```py
ur.degree(edges, direction="in")
ur.hop(edges, n=2, direction="both").from_([0])
```

## Kernels with a fixed direction

Some algorithms are defined on directed graphs only, and some on undirected graphs only. These take no `direction` parameter.

| Algorithm | Reads the rows as |
|---|---|
| `pagerank`, `betweenness`, `closeness`, `connected_components(mode="strong")` | directed |
| `triangle_count`, `clustering_coefficient`, `connected_components(mode="weak")`, `louvain`, `label_propagation` | undirected |

## Undirected data

If your data is undirected and each edge appears once, the directed kernels see only one direction. The result is correct for the rows as stored, but it can differ from a published undirected result.

To get the undirected result from a directed kernel, store each edge in both directions. The example adds the reverse rows with pyarrow:

```python
import pyarrow as pa
import ursa as ur

edges = ur.datasets.load_karate()
t = edges.collect().to_arrow()
both = ur.EdgeFrame(
    pa.concat_tables([t, t.select(["dst", "src"]).rename_columns(["src", "dst"])]),
    src="src",
    dst="dst",
)
print(ur.describe(edges).collect())
print(ur.describe(both).collect())
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
┌─────────┬─────────┬──────────┬────────────┬──────────────┐
│ n_nodes ┆ n_edges ┆ density  ┆ avg_degree ┆ n_components │
│ ---     ┆ ---     ┆ ---      ┆ ---        ┆ ---          │
│ int64   ┆ int64   ┆ double   ┆ double     ┆ int64        │
╞═════════╪═════════╪══════════╪════════════╪══════════════╡
│ 34      ┆ 156     ┆ 0.139037 ┆ 4.58824    ┆ null         │
└─────────┴─────────┴──────────┴────────────┴──────────────┘
```

!!! tip

    For `degree`, `hop`, and `shortest_path`, pass `direction="both"` instead. It gives the undirected result with no change to the data.

See [Semantics vs NetworkX](../semantics.md) for the exact NetworkX call each kernel matches.
