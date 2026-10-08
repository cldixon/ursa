# Expressions

An expression describes a column. You pass expressions to `with_columns`, `filter`, `sort`, `group_by().agg()`, and to the `weight=` parameter of an algorithm. Expressions are pure Python values. They do no work until `collect()`.

## Column references

| Expression | Meaning |
|---|---|
| `ur.col("name")` | The column `name` of the frame the expression runs in. |
| `ur.lit(3.5)` | A constant. |
| `ur.src()` | The source column of the ambient `EdgeFrame`, whatever its name. |
| `ur.dst()` | The destination column of the ambient `EdgeFrame`. |
| `ur.id()` | The id column of the ambient `NodeFrame`. |

## Operators

Expressions combine with Python operators:

```py
ur.col("amount") * ur.col("fx")            # arithmetic: + - * /
ur.col("degree") > 2                       # comparison: > >= < <= == !=
(ur.col("a") > 1) & ~(ur.col("b") == "x")  # boolean: & | ~
```

A plain Python number or string on one side of an operator becomes a literal. `ur.col("degree") > 2` and `ur.col("degree") > ur.lit(2)` are the same expression.

!!! note "Use `&`, `|`, and `~`"

    Python's `and`, `or`, and `not` do not work on expressions. Use `&`, `|`, and `~`, and put parentheses around each comparison.

## Aggregations

Inside `group_by().agg()` and `neighbors().agg()`, an expression can end with an aggregation:

```py
ur.col("capacity").sum()
ur.col("seniority").mean()
ur.col("team").n_unique()
```

The aggregations are `mean`, `sum`, `min`, `max`, `count`, and `n_unique`. Add `.alias("name")` to name the output column, or pass the expression as a keyword argument: `.agg(total=ur.col("capacity").sum())`.

## Graph algorithms are expressions

`ur.pagerank(edges)` returns an expression that names an algorithm over `edges`. It has two uses:

- Inside `with_columns`, it is the definition of a column.
- On its own, it acts as a `NodeFrame` of `(id, pagerank)`. You can call `collect()`, `filter()`, `sort()`, or `head()` on it directly.

```py
nodes.with_columns(pr=ur.pagerank(edges))   # a column
ur.pagerank(edges).sort("pagerank").head(5)  # a frame
```

When one `with_columns` call names the same algorithm twice with the same parameters, the engine runs it once.

## Where a column resolves

`ur.col("x")` resolves against the frame the expression runs in. In `nodes.with_columns(...)`, that is `nodes`. Inside `ur.neighbors(edges).agg(...)`, the attribute still comes from the ambient `NodeFrame`, while the topology comes from `edges`. In `weight=`, the column resolves against the `EdgeFrame` of the algorithm.
