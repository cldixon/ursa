# Quickstart

This page is one Python session. It uses a bundled dataset, so you need no files and no network. Each block runs on its own after the first one.

## 1. Load a graph

```python
import ursa as ur

edges, nodes = ur.datasets.load_karate(with_nodes=True)
print(edges.src_col, edges.dst_col, nodes.id_col)
```

```text
src dst id
```

`edges` is an `EdgeFrame`: the graph. Each row is one edge from the `src` column to the `dst` column. `nodes` is a `NodeFrame`: a table of node attributes. Here the attribute is `club`, the faction each member joined.

Both frames are lazy. `collect()` reads the rows:

```python
print(edges.collect())
```

```text
shape: (78, 2)
┌───────┬───────┐
│ src   ┆ dst   │
│ ---   ┆ ---   │
│ int64 ┆ int64 │
╞═══════╪═══════╡
│ 0     ┆ 1     │
│ 0     ┆ 2     │
│ 0     ┆ 3     │
│ 0     ┆ 4     │
│ 0     ┆ 5     │
│ 0     ┆ 6     │
│ 0     ┆ 7     │
│ 0     ┆ 8     │
│ 0     ┆ 10    │
│ 0     ┆ 11    │
│ …     ┆ …     │
└───────┴───────┘
```

## 2. Compute a metric

A graph algorithm is a function of the edges. On its own, it is a table of `(id, value)`:

```python
print(ur.pagerank(edges).sort("pagerank", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬───────────┐
│ id    ┆ pagerank  │
│ ---   ┆ ---       │
│ int64 ┆ double    │
╞═══════╪═══════════╡
│ 33    ┆ 0.259047  │
│ 32    ┆ 0.0954893 │
│ 31    ┆ 0.0459255 │
└───────┴───────────┘
```

!!! note "Direction"

    Every edge row is directed. The karate graph is undirected, and the dataset stores each edge once. `pagerank` follows the rows as stored, so its result differs from a published undirected PageRank. `degree`, `hop`, and `shortest_path` accept `direction="both"` for the undirected result. See [Direction](concepts/direction.md).

## 3. Add metrics to your node table

Inside `with_columns`, an algorithm is a column definition. Ursa joins the values to your table by id, runs everything as one query, and keeps every row of your table.

```python
ranked = nodes.with_columns(
    degree=ur.degree(edges, direction="both"),
    triangles=ur.triangle_count(edges),
    clustering=ur.clustering_coefficient(edges),
)
print(ranked.sort("degree", descending=True).head(5).collect())
```

```text
shape: (5, 5)
┌───────┬─────────┬────────┬───────────┬────────────┐
│ id    ┆ club    ┆ degree ┆ triangles ┆ clustering │
│ ---   ┆ ---     ┆ ---    ┆ ---       ┆ ---        │
│ int64 ┆ string  ┆ uint32 ┆ uint32    ┆ double     │
╞═══════╪═════════╪════════╪═══════════╪════════════╡
│ 33    ┆ Officer ┆ 17     ┆ 15        ┆ 0.110294   │
│ 0     ┆ Mr. Hi  ┆ 16     ┆ 18        ┆ 0.15       │
│ 32    ┆ Officer ┆ 12     ┆ 13        ┆ 0.19697    │
│ 2     ┆ Mr. Hi  ┆ 10     ┆ 11        ┆ 0.244444   │
│ 1     ┆ Mr. Hi  ┆ 9      ┆ 12        ┆ 0.333333   │
└───────┴─────────┴────────┴───────────┴────────────┘
```

## 4. Filter and aggregate

Filters are expressions over any column, computed or not:

```python
officers = ranked.filter((ur.col("club") == "Officer") & (ur.col("degree") >= 9))
print(officers.collect())
```

```text
shape: (2, 5)
┌───────┬─────────┬────────┬───────────┬────────────┐
│ id    ┆ club    ┆ degree ┆ triangles ┆ clustering │
│ ---   ┆ ---     ┆ ---    ┆ ---       ┆ ---        │
│ int64 ┆ string  ┆ uint32 ┆ uint32    ┆ double     │
╞═══════╪═════════╪════════╪═══════════╪════════════╡
│ 32    ┆ Officer ┆ 12     ┆ 13        ┆ 0.19697    │
│ 33    ┆ Officer ┆ 17     ┆ 15        ┆ 0.110294   │
└───────┴─────────┴────────┴───────────┴────────────┘
```

`group_by` summarizes the table:

```python
by_club = ranked.group_by("club").agg(
    members=ur.col("id").count(),
    mean_degree=ur.col("degree").mean(),
)
print(by_club.collect())
```

```text
shape: (2, 3)
┌─────────┬─────────┬─────────────┐
│ club    ┆ members ┆ mean_degree │
│ ---     ┆ ---     ┆ ---         │
│ string  ┆ int64   ┆ double      │
╞═════════╪═════════╪═════════════╡
│ Mr. Hi  ┆ 17      ┆ 4.76471     │
│ Officer ┆ 17      ┆ 4.41176     │
└─────────┴─────────┴─────────────┘
```

## 5. Look at the neighbours

`neighbors().agg()` aggregates an attribute over each node's neighbours. Here it counts the clubs among a member's friends. A count of 2 marks a member with friends in both factions.

```python
bridges = (
    nodes.with_columns(
        clubs_around=ur.neighbors(edges, direction="both").agg(ur.col("club").n_unique()),
    )
    .filter(ur.col("clubs_around") == 2)
)
print(bridges.collect())
```

```text
shape: (13, 3)
┌───────┬─────────┬──────────────┐
│ id    ┆ club    ┆ clubs_around │
│ ---   ┆ ---     ┆ ---          │
│ int64 ┆ string  ┆ double       │
╞═══════╪═════════╪══════════════╡
│ 0     ┆ Mr. Hi  ┆ 2            │
│ 1     ┆ Mr. Hi  ┆ 2            │
│ 2     ┆ Mr. Hi  ┆ 2            │
│ 8     ┆ Mr. Hi  ┆ 2            │
│ 13    ┆ Mr. Hi  ┆ 2            │
│ 19    ┆ Mr. Hi  ┆ 2            │
│ 31    ┆ Officer ┆ 2            │
│ 30    ┆ Officer ┆ 2            │
│ 9     ┆ Officer ┆ 2            │
│ 27    ┆ Officer ┆ 2            │
│ …     ┆ …       ┆ …            │
└───────┴─────────┴──────────────┘
```

## 6. Traverse

`hop` gives the nodes within `n` steps of a seed. The result is an `EdgeFrame` of `(seed, reached)` pairs.

```python
near = ur.hop(edges, n=1, direction="both").from_([0])
print(near.sort("dst").head(5).collect())
```

```text
shape: (5, 2)
┌───────┬───────┐
│ src   ┆ dst   │
│ ---   ┆ ---   │
│ int64 ┆ int64 │
╞═══════╪═══════╡
│ 0     ┆ 1     │
│ 0     ┆ 2     │
│ 0     ┆ 3     │
│ 0     ┆ 4     │
│ 0     ┆ 5     │
└───────┴───────┘
```

`shortest_path` gives one path, one row per edge:

```python
print(ur.shortest_path(edges, 0, 33, direction="both").collect())
```

```text
shape: (2, 4)
┌───────┬───────┬───────┬────────┐
│ src   ┆ dst   ┆ hop   ┆ cost   │
│ ---   ┆ ---   ┆ ---   ┆ ---    │
│ int64 ┆ int64 ┆ int64 ┆ double │
╞═══════╪═══════╪═══════╪════════╡
│ 0     ┆ 8     ┆ 0     ┆ 1      │
│ 8     ┆ 33    ┆ 1     ┆ 2      │
└───────┴───────┴───────┴────────┘
```

## 7. Summarize

```python
print(ur.describe(edges).collect())
print("density:", ur.density(edges))
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
density: 0.06951871657754011
```

## 8. Get the result out

A collected frame is Arrow data. Hand it to polars, to pyarrow, or to a file.

```python
result = ranked.sort("degree", descending=True).collect()

table = result.to_arrow()        # pyarrow.Table, no copy
rows = result.to_dicts()         # list of dicts
result.sink_parquet("karate_metrics.parquet")

print(type(table).__name__, rows[0])
```

```text
Table {'id': 33, 'club': 'Officer', 'degree': 17, 'triangles': 15, 'clustering': 0.11029411764705882}
```

With the `polars` extra installed, `result.to_polars()` gives a `polars.DataFrame`.

## 9. Use your own data

Replace the first line of this session with a scan. The rest does not change.

```py
edges = ur.scan_edges("edges.parquet", src="from_id", dst="to_id")
nodes = ur.scan_nodes("nodes.parquet", id="node_id")
```

A scan reads the file inside `collect()`, and reads only the columns the query needs. Paths can be local, `s3://`, `gs://`, `az://`, or `https://`. See [Load data](guides/load-data.md).

## Where next

- The [guides](guides/index.md) cover each task in more depth.
- [Semantics vs NetworkX](semantics.md) pins every kernel to the NetworkX call it matches.
- [Limits](limits.md) lists what does not run yet.
