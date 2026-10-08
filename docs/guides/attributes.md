# Use node attributes

A `NodeFrame` is a table of node attributes. Ursa joins computed values to it by id, and your attributes take part in filters, sorts, groups, and neighbour aggregations.

```python
import ursa as ur

edges, nodes = ur.datasets.load_karate(with_nodes=True)
print(nodes.head(3).collect())
```

```text
shape: (3, 2)
┌───────┬────────┐
│ id    ┆ club   │
│ ---   ┆ ---    │
│ int64 ┆ string │
╞═══════╪════════╡
│ 0     ┆ Mr. Hi │
│ 1     ┆ Mr. Hi │
│ 2     ┆ Mr. Hi │
└───────┴────────┘
```

## Add computed columns to your table

`with_columns` on a `NodeFrame` left-joins the algorithm output by id. Every row of your table stays, and your columns come first.

```python
ranked = nodes.with_columns(degree=ur.degree(edges, direction="both"))
print(ranked.sort("degree", descending=True).head(3).collect())
```

```text
shape: (3, 3)
┌───────┬─────────┬────────┐
│ id    ┆ club    ┆ degree │
│ ---   ┆ ---     ┆ ---    │
│ int64 ┆ string  ┆ uint32 │
╞═══════╪═════════╪════════╡
│ 33    ┆ Officer ┆ 17     │
│ 0     ┆ Mr. Hi  ┆ 16     │
│ 32    ┆ Officer ┆ 12     │
└───────┴─────────┴────────┘
```

A node in your table that is not in the edges gets a null in every computed column. A node in the edges but not in your table does not appear.

## Filter and sort on attributes

An attribute column and a computed column work the same way in an expression.

```python
print(
    ranked.filter((ur.col("club") == "Mr. Hi") & (ur.col("degree") > 6))
    .sort("degree", descending=True)
    .collect()
)
```

```text
shape: (3, 3)
┌───────┬────────┬────────┐
│ id    ┆ club   ┆ degree │
│ ---   ┆ ---    ┆ ---    │
│ int64 ┆ string ┆ uint32 │
╞═══════╪════════╪════════╡
│ 0     ┆ Mr. Hi ┆ 16     │
│ 2     ┆ Mr. Hi ┆ 10     │
│ 1     ┆ Mr. Hi ┆ 9      │
└───────┴────────┴────────┘
```

## Aggregate over neighbours

`ur.neighbors(edges).agg(expr)` computes, for each node, an aggregation of an attribute over its neighbours. The topology comes from `edges`. The attribute comes from the table the expression runs in.

Here, `n_unique` over `club` counts the factions among each member's friends:

```python
around = nodes.with_columns(
    clubs_around=ur.neighbors(edges, direction="both").agg(ur.col("club").n_unique()),
)
print(around.filter(ur.col("clubs_around") == 2).collect())
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

Numeric attributes take `mean`, `sum`, `min`, and `max`:

```python
people = ur.NodeFrame(
    {"id": [0, 1, 2, 3], "age": [30, 40, 50, 60]},
    id="id",
)
links = ur.from_edgelist([(0, 1), (0, 2), (1, 3), (2, 3)])

print(
    people.with_columns(
        mean_age_out=ur.neighbors(links, direction="out").agg(ur.col("age").mean()),
        max_age_in=ur.neighbors(links, direction="in").agg(ur.col("age").max()),
    ).collect()
)
```

```text
shape: (4, 4)
┌───────┬───────┬──────────────┬────────────┐
│ id    ┆ age   ┆ mean_age_out ┆ max_age_in │
│ ---   ┆ ---   ┆ ---          ┆ ---        │
│ int64 ┆ int64 ┆ double       ┆ double     │
╞═══════╪═══════╪══════════════╪════════════╡
│ 0     ┆ 30    ┆ 45           ┆ null       │
│ 1     ┆ 40    ┆ 60           ┆ 30         │
│ 2     ┆ 50    ┆ 60           ┆ 30         │
│ 3     ┆ 60    ┆ null         ┆ 50         │
└───────┴───────┴──────────────┴────────────┘
```

A node with no neighbours in the given direction gets a null.

## Node tables from a file

`ur.scan_nodes` reads a node table from Parquet or CSV. The join and the filters are the same. When the plan uses a few columns of a wide Parquet file, the scan reads only those columns.

```py
nodes = ur.scan_nodes("accounts.parquet", id="account_id")
edges = ur.scan_edges("transfers.parquet", src="from_account", dst="to_account")

flagged = (
    nodes.with_columns(in_degree=ur.degree(edges, direction="in"))
    .filter((ur.col("country") == "NL") & (ur.col("in_degree") > 100))
    .select("account_id", "in_degree")
)
```

## Select the output columns

`select` keeps only the named columns. Call it after `with_columns`.

```python
print(ranked.select("id", "degree").head(3).collect())
```

```text
shape: (3, 2)
┌───────┬────────┐
│ id    ┆ degree │
│ ---   ┆ ---    │
│ int64 ┆ uint32 │
╞═══════╪════════╡
│ 0     ┆ 16     │
│ 1     ┆ 9      │
│ 2     ┆ 10     │
└───────┴────────┘
```
