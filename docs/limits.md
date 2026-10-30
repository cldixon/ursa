# Limits

Ursa is version 0.4. The API is stable in shape, and some combinations of steps do not run yet. When a plan hits one, the step that adds it raises `NotImplementedError` with a message that says what to change (a few, such as a scan of files on different stores, raise when `collect()` reads them). Nothing is dropped silently.

## Combinations that do not run

| Combination | Status |
|---|---|
| A scan of a list of paths on different stores (for example a local file and an `s3://` file) | Put the files on one store, or read them into memory first. |
| `edges.nodes().collect()` with no columns | Add a column first, for example `with_columns(degree=ur.degree(edges))`. |

## Scope

- One machine, in memory. The graph index must fit in RAM.
- Node ids are `int64` or `string`. One frame uses one kind.
- There is no graph mutation. To change the graph, build a new frame.
- There is no visualization. Positions and drawing are out of scope for 0.4.

## Report a gap

If a limit blocks you, open an issue at [github.com/cldixon/ursa/issues](https://github.com/cldixon/ursa/issues) with the plan from `explain()`.
