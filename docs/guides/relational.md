# Shape the result

The row and column methods work on every frame: an edge table, a node table, an algorithm result, or a traversal result. They add steps to the plan and run inside `collect()`.

The steps run in the order you write them, as in Polars. `sort(...).head(3)` keeps the top three rows; `head(3).sort(...)` sorts the first three. A step sees the columns the steps before it leave: after `rename`, later steps use the new names; after `select`, only the selected columns.

```python
import ursa as ur

edges, nodes = ur.datasets.load_karate(with_nodes=True)
ranked = nodes.with_columns(
    degree=ur.degree(edges, direction="both"),
    triangles=ur.triangle_count(edges),
)
```

## Filter

A predicate is a boolean [expression](../concepts/expressions.md). Combine comparisons with `&`, `|`, and `~`.

```python
print(ranked.filter((ur.col("degree") > 5) & ~(ur.col("club") == "Officer")).collect())
```

```text
shape: (4, 4)
┌───────┬────────┬────────┬───────────┐
│ id    ┆ club   ┆ degree ┆ triangles │
│ ---   ┆ ---    ┆ ---    ┆ ---       │
│ int64 ┆ string ┆ uint32 ┆ uint32    │
╞═══════╪════════╪════════╪═══════════╡
│ 0     ┆ Mr. Hi ┆ 16     ┆ 18        │
│ 1     ┆ Mr. Hi ┆ 9      ┆ 12        │
│ 2     ┆ Mr. Hi ┆ 10     ┆ 11        │
│ 3     ┆ Mr. Hi ┆ 6      ┆ 10        │
└───────┴────────┴────────┴───────────┘
```

## Sort and head

`sort` takes a column name or a list of names. `head(n)` keeps the first `n` rows. Written after a `sort`, those are the top rows.

```python
print(ranked.sort("triangles", descending=True).head(3).collect())
```

```text
shape: (3, 4)
┌───────┬─────────┬────────┬───────────┐
│ id    ┆ club    ┆ degree ┆ triangles │
│ ---   ┆ ---     ┆ ---    ┆ ---       │
│ int64 ┆ string  ┆ uint32 ┆ uint32    │
╞═══════╪═════════╪════════╪═══════════╡
│ 0     ┆ Mr. Hi  ┆ 16     ┆ 18        │
│ 33    ┆ Officer ┆ 17     ┆ 15        │
│ 32    ┆ Officer ┆ 12     ┆ 13        │
└───────┴─────────┴────────┴───────────┘
```

With several columns, later columns break ties in earlier ones. `descending` is one bool for every column, or a list with one bool per column. Nulls sort last for every column.

```python
print(ranked.sort(["club", "degree", "id"], descending=[False, True, False]).head(4).collect())
```

```text
shape: (4, 4)
┌───────┬────────┬────────┬───────────┐
│ id    ┆ club   ┆ degree ┆ triangles │
│ ---   ┆ ---    ┆ ---    ┆ ---       │
│ int64 ┆ string ┆ uint32 ┆ uint32    │
╞═══════╪════════╪════════╪═══════════╡
│ 0     ┆ Mr. Hi ┆ 16     ┆ 18        │
│ 2     ┆ Mr. Hi ┆ 10     ┆ 11        │
│ 1     ┆ Mr. Hi ┆ 9      ┆ 12        │
│ 3     ┆ Mr. Hi ┆ 6      ┆ 10        │
└───────┴────────┴────────┴───────────┘
```

Rows that tie on every sort column keep their input order, on every run. On a plain frame that is the row order. After a graph algorithm, as here, it is the attribute table's row order, or node order for an `edges.nodes()` frame. After a traversal it is the order the traversal returns its rows in.

The same input order is what you get without a `sort`. An unsorted `collect()` of `ranked` returns the attribute table's rows in their order, and `head(n)` keeps the first `n` of them. A `join` returns the left rows in order, each followed by its matches in right-frame order, and `group_by` and `distinct` return their rows in order of first appearance.

## Select and rename

`select` keeps the named columns, in the given order. Call it after `with_columns`.

```python
print(ranked.select("id", "degree").sort("degree", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬────────┐
│ id    ┆ degree │
│ ---   ┆ ---    │
│ int64 ┆ uint32 │
╞═══════╪════════╡
│ 33    ┆ 17     │
│ 0     ┆ 16     │
│ 32    ┆ 12     │
└───────┴────────┘
```

`rename` changes column names. The steps after it use the new names:

```python
print(ranked.rename({"id": "member", "degree": "friends"}).sort("friends", descending=True).head(3).collect())
```

```text
shape: (3, 4)
┌────────┬─────────┬─────────┬───────────┐
│ member ┆ club    ┆ friends ┆ triangles │
│ ---    ┆ ---     ┆ ---     ┆ ---       │
│ int64  ┆ string  ┆ uint32  ┆ uint32    │
╞════════╪═════════╪═════════╪═══════════╡
│ 33     ┆ Officer ┆ 17      ┆ 15        │
│ 0      ┆ Mr. Hi  ┆ 16      ┆ 18        │
│ 32     ┆ Officer ┆ 12      ┆ 13        │
└────────┴─────────┴─────────┴───────────┘
```

## Group by

`group_by(*keys).agg(...)` gives one row per group. The output has the keys, then the aggregations. Name an aggregation with a keyword or with `.alias()`.

```python
print(
    ranked.group_by("club")
    .agg(
        ur.col("triangles").sum().alias("triangles"),
        members=ur.col("id").count(),
        mean_degree=ur.col("degree").mean(),
    )
    .collect()
)
```

```text
shape: (2, 4)
┌─────────┬───────────┬─────────┬─────────────┐
│ club    ┆ triangles ┆ members ┆ mean_degree │
│ ---     ┆ ---       ┆ ---     ┆ ---         │
│ string  ┆ uint64    ┆ int64   ┆ double      │
╞═════════╪═══════════╪═════════╪═════════════╡
│ Officer ┆ 52        ┆ 17      ┆ 4.41176     │
│ Mr. Hi  ┆ 83        ┆ 17      ┆ 4.76471     │
└─────────┴───────────┴─────────┴─────────────┘
```

`group_by` works on an edge table too. Out-degree is a count of rows per source:

```python
print(edges.group_by("src").agg(out_degree=ur.col("dst").count()).sort("out_degree", descending=True).head(3).collect())
```

```text
shape: (3, 2)
┌───────┬────────────┐
│ src   ┆ out_degree │
│ ---   ┆ ---        │
│ int64 ┆ int64      │
╞═══════╪════════════╡
│ 0     ┆ 16         │
│ 1     ┆ 8          │
│ 2     ┆ 8          │
└───────┴────────────┘
```

## Join

`join(other, on=, how=)` is an equi-join on shared key columns. `how` is `"inner"` or `"left"`. It joins two tables you built; the automatic join of an algorithm result onto a `NodeFrame` does not need it.

```python
extra = ur.NodeFrame({"id": [0, 1, 2], "joined": [1970, 1971, 1972]}, id="id")
print(nodes.join(extra, on="id", how="inner").collect())
```

```text
shape: (3, 3)
┌───────┬────────┬────────┐
│ id    ┆ club   ┆ joined │
│ ---   ┆ ---    ┆ ---    │
│ int64 ┆ string ┆ int64  │
╞═══════╪════════╪════════╡
│ 0     ┆ Mr. Hi ┆ 1970   │
│ 1     ┆ Mr. Hi ┆ 1971   │
│ 2     ┆ Mr. Hi ┆ 1972   │
└───────┴────────┴────────┘
```

`join` and `group_by` compose in one plan, in the order you write them. `join(...).group_by(...)` groups the joined rows. `group_by(...).agg(...).join(...)` joins onto the groups. A `filter` after `group_by(...).agg(...)` filters the groups, like SQL `HAVING`:

```python
leaders = ur.NodeFrame({"club": ["Mr. Hi", "Officer"], "leader": [0, 33]}, id="club")
print(
    ranked.group_by("club")
    .agg(members=ur.col("id").count(), mean_degree=ur.col("degree").mean())
    .join(leaders, on="club")
    .sort("club")
    .collect()
)
```

```text
shape: (2, 4)
┌─────────┬─────────┬─────────────┬────────┐
│ club    ┆ members ┆ mean_degree ┆ leader │
│ ---     ┆ ---     ┆ ---         ┆ ---    │
│ string  ┆ int64   ┆ double      ┆ int64  │
╞═════════╪═════════╪═════════════╪════════╡
│ Mr. Hi  ┆ 17      ┆ 4.76471     ┆ 0      │
│ Officer ┆ 17      ┆ 4.41176     ┆ 33     │
└─────────┴─────────┴─────────────┴────────┘
```

## Distinct and sample

`distinct()` removes duplicate rows. After a `select`, that is duplicates of the selected columns. `sample(n, seed=)` keeps `n` random rows.

```python
print(nodes.select("club").distinct().sort("club").collect())
print(nodes.sample(2, seed=3).collect())
```

```text
shape: (2, 1)
┌─────────┐
│ club    │
│ ---     │
│ string  │
╞═════════╡
│ Mr. Hi  │
│ Officer │
└─────────┘
shape: (2, 2)
┌───────┬─────────┐
│ id    ┆ club    │
│ ---   ┆ ---     │
│ int64 ┆ string  │
╞═══════╪═════════╡
│ 8     ┆ Mr. Hi  │
│ 15    ┆ Officer │
└───────┴─────────┘
```

## Explain

`explain()` prints the plan and runs nothing.

```python
print(ranked.filter(ur.col("degree") > 5).sort("degree", descending=True).head(3).explain())
```

```text
NodeFrame plan [index: dropped (rebuild lazily)]
  0: from_arrow(id='id')
  1: with_columns(exprs={'degree': degree(...), 'triangles': triangle_count(...)})
  2: filter(predicate=(col('degree') > lit(5)))
  3: sort(by='degree', descending=True)
  4: head(n=3)
```

Some combinations of steps do not run yet. `collect()` raises `NotImplementedError` with the fix. See [Limits](../limits.md).
