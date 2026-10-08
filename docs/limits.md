---
icon: lucide/construction
---

# Limits

Ursa is version 0.3. The API is stable in shape, and some combinations of steps do not run yet. When a plan hits one, `collect()` raises `NotImplementedError` with a message that says what to change. Nothing is dropped silently.

## Steps that do not run

| Step | Status |
|---|---|
| `frame.schema()` | Not available. Use `collect().columns` or `explain()`. |
| `frame.sort(by)` with an expression or several columns | Only one column name. Sort in polars after `collect()` for more. |
| `frame.select(...)` with an expression | Only names and `ur.col(name)`. Compute the column in `with_columns` first. |
| CSV options on a scan, such as a delimiter | Not available. The file must be comma-separated with a header. |

## Combinations that do not run

| Combination | Status |
|---|---|
| Two `with_columns` steps in one plan | One per plan. Put every column in one call. |
| `with_columns` after `select` | Compute first, then `select`. |
| Two `select` steps | One per plan. |
| `filter` after `group_by().agg()` | Filter before the `group_by`. |
| `group_by().agg()` with `distinct`, `sample`, or `select` | Not available. `sort`, `head`, and `rename` work on the grouped result. |
| `join` with `group_by().agg()` | Do them in separate `collect()` calls. |
| Two `join` steps | One per plan. |
| A weighted algorithm over a scan with several paths | Pass one path, or read the files into memory first. |
| A tail (`filter`, `sort`, `head`) after `describe()` | Collect `describe()` on its own. |
| A graph algorithm over a `distinct`, `sample`, `join`, or `group_by` result | Collect the rows and build a new frame. A `filter` on the edges works directly. See [Reshaped edge sets](guides/subgraphs.md#reshaped-edge-sets). |
| `group_by().agg()` on a `hop` or `shortest_path` result | Collect the rows and build a new frame. |
| `filter`, `sort`, or `select` on a renamed column | A rename applies last in a plan. Use the old name in the other steps. |
| `edges.nodes().collect()` with no columns | Add a column first, for example `with_columns(degree=ur.degree(edges))`. |

## Scope

- One machine, in memory. The graph index must fit in RAM.
- Node ids are `int64` or `string`. One frame uses one kind.
- There is no graph mutation. To change the graph, build a new frame.
- There is no visualization. Positions and drawing are out of scope for 0.3.

## Report a gap

If a limit blocks you, open an issue at [github.com/cldixon/ursa/issues](https://github.com/cldixon/ursa/issues) with the plan from `explain()`.
