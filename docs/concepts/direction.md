# Direction

Ursa has no `Graph` and `DiGraph` split. Every edge row is directed, from `src` to `dst`. Direction is a parameter of each operation.

## The `direction` parameter

Operations that walk edges accept `direction`:

| Value | Follows |
|---|---|
| `"out"` (default) | edges from the node: `src → dst` |
| `"in"` | edges into the node: `dst → src` |
| `"both"` | edges in either direction |

`degree`, `neighbors`, `hop`, `shortest_path`, `closeness`, and the statistics (`density`, `avg_path_length`, `diameter`, `describe`) accept all three values. `pagerank` and `betweenness` accept `"out"` and `"both"`.

```py
ur.degree(edges, direction="in")
ur.hop(edges, n=2, direction="both").from_([0])
ur.pagerank(edges, direction="both")
```

For `pagerank`, `betweenness`, and the statistics, `"both"` means the simple undirected graph: one edge for each distinct pair of nodes joined by a row in either direction, with self-loops dropped. That is NetworkX's `G.to_undirected()`.

## Kernels with a fixed direction

Some algorithms are defined on one kind of graph only. These take no `direction` parameter.

| Algorithm | Reads the rows as |
|---|---|
| `connected_components(mode="strong")` | directed |
| `triangle_count`, `clustering_coefficient`, `connected_components(mode="weak")`, `louvain`, `label_propagation` | undirected |

## Undirected data

If your data is undirected and each edge appears once, the default `direction="out"` sees only one direction. The result is correct for the rows as stored, but it can differ from a published undirected result. Pass `direction="both"`:

```python
import ursa as ur

edges = ur.datasets.load_karate()
print(ur.diameter(edges, approximate=False), ur.diameter(edges, approximate=False, direction="both"))
```

```text
3 5
```

You can also store each edge in both directions. Then every kernel's default sees the undirected graph, except `betweenness`, which counts each pair from both ends and so gives twice the undirected value; pass it `direction="both"`. `ur.from_networkx` does this for an undirected `nx.Graph`. The example adds the reverse rows with pyarrow:

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

    `ur.describe(edges, direction="both")` gives the undirected figures with no change to the data: 78 edges, density 0.139037, mean degree 4.58824.

See [Semantics vs NetworkX](../semantics.md) for the exact NetworkX call each kernel matches.
