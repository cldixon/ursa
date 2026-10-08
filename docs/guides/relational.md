# Shape the result

The row and column methods work on every frame: an edge table, a node table, an algorithm result, or a traversal result. They add steps to the plan and run inside `collect()`.

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

`sort` takes one column name. `head(n)` keeps the first `n` rows after the sort.

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

`rename` changes column names. It applies last in a plan, so `filter`, `sort`, and `select` in the same plan use the old names.

```python
print(ranked.rename({"id": "member", "degree": "friends"}).head(3).collect())
```

```text
shape: (3, 4)
┌────────┬────────┬─────────┬───────────┐
│ member ┆ club   ┆ friends ┆ triangles │
│ ---    ┆ ---    ┆ ---     ┆ ---       │
│ int64  ┆ string ┆ uint32  ┆ uint32    │
╞════════╪════════╪═════════╪═══════════╡
│ 0      ┆ Mr. Hi ┆ 16      ┆ 18        │
│ 1      ┆ Mr. Hi ┆ 9       ┆ 12        │
│ 2      ┆ Mr. Hi ┆ 10      ┆ 11        │
└────────┴────────┴─────────┴───────────┘
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

## Distinct and sample

`distinct()` removes duplicate rows. `sample(n, seed=)` keeps `n` random rows.

```python
print(nodes.select("club").distinct().collect())
print(nodes.sample(2, seed=3).collect())
```

```text
shape: (34, 1)
┌─────────┐
│ club    │
│ ---     │
│ string  │
╞═════════╡
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Mr. Hi  │
│ Officer │
│ …       │
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
