# Expressions

An expression is a value of type `ur.Expr`. See [Concepts: Expressions](../concepts/expressions.md) for how they are used.

## Constructors

| Function | Expression |
|---|---|
| `ur.col(name: str)` | The column `name`. |
| `ur.lit(value)` | A constant: a number, a string, or a boolean. |
| `ur.src()` | The source column of the ambient `EdgeFrame`. |
| `ur.dst()` | The destination column of the ambient `EdgeFrame`. |
| `ur.id()` | The id column of the ambient `NodeFrame`. |

`ur.src()`, `ur.dst()`, and `ur.id()` resolve only where a frame with that role is ambient. Elsewhere they raise `NotImplementedError`.

## Operators

| Kind | Operators | Result |
|---|---|---|
| Arithmetic | `+`, `-`, `*`, `/` | numeric |
| Comparison | `>`, `>=`, `<`, `<=`, `==`, `!=` | boolean |
| Boolean | `&`, `|`, `~` | boolean |

Each operator accepts an expression or a plain Python value on either side. A plain value becomes a literal.

A `filter` predicate is any boolean expression. A comparison can compare a column with a literal or with another column.

## Aggregations

| Method | Meaning |
|---|---|
| `.mean()` | The arithmetic mean. |
| `.sum()` | The sum. |
| `.min()` | The minimum. |
| `.max()` | The maximum. |
| `.count()` | The number of non-null values. |
| `.n_unique()` | The number of distinct values. |

An aggregation is valid inside `group_by().agg()` and `neighbors().agg()`.

## Naming

| Method | Meaning |
|---|---|
| `.alias(name: str)` | Names the output column. |

Without an alias, an aggregation in `group_by().agg()` is named after its column. A keyword argument also names the output: `.agg(total=ur.col("x").sum())`.

## `repr`

`repr(expr)` prints the expression tree. `frame.explain()` prints the expressions in a plan the same way.
