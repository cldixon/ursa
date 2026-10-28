# Limits

Ursa is version 0.4. The API is stable in shape, and some combinations of steps do not run yet. When a plan hits one, `collect()` raises `NotImplementedError` with a message that says what to change. Nothing is dropped silently.

## Steps that do not run

| Step | Status |
|---|---|
| `frame.schema()` | Not available. Use `collect().columns` or `explain()`. |
| CSV options on a scan, such as a delimiter | Not available. The file must be comma-separated with a header. |

## Combinations that do not run

| Combination | Status |
|---|---|
| A step other than `filter` on an `EdgeFrame` before `nodes()` | Collect the edges and build a new frame. |
| A weighted algorithm over a scan with several paths | Pass one path, or read the files into memory first. |
| A graph algorithm over a `distinct`, `sample`, `join`, or `group_by` result | Collect the rows and build a new frame. A `filter` on the edges works directly. See [Reshaped edge sets](guides/subgraphs.md#reshaped-edge-sets). |
| `edges.nodes().collect()` with no columns | Add a column first, for example `with_columns(degree=ur.degree(edges))`. |

## Scope

- One machine, in memory. The graph index must fit in RAM.
- Node ids are `int64` or `string`. One frame uses one kind.
- There is no graph mutation. To change the graph, build a new frame.
- There is no visualization. Positions and drawing are out of scope for 0.4.

## Report a gap

If a limit blocks you, open an issue at [github.com/cldixon/ursa/issues](https://github.com/cldixon/ursa/issues) with the plan from `explain()`.
