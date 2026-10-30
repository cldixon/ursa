# Limits

Ursa is version 0.4. The API is stable in shape, and some combinations of steps do not run yet. When a plan hits one, the step that adds it raises `NotImplementedError` with a message that says what to change (a few, such as a scan of files on different stores, raise when `collect()` reads them). Nothing is dropped silently.

## Combinations that do not run

| Combination | Status |
|---|---|
| A traversal (`hop`, `shortest_path`) of a traversal result | Collect the first result and build a new frame with `ur.from_arrow(...)`. |
| A step between a traversal and a graph op over it, such as `ur.degree(ur.hop(e, 1).from_([0]).filter(...))` | Run the graph op over the bare traversal, or collect the traversal and build a new frame. |
| `reverse()` after a traversal | Reverse the edges before the traversal. |
| `with_columns` after `random_walk()` or `describe()` | Collect the result and build a new frame. Other relational steps and joins work. |
| `ur.neighbors(edges, from_=...)` | The attribute comes from the frame the aggregation runs in. Join the attributes onto that frame first. |
| A frame with more than about 65,000 relational steps (counting the steps of the frames a graph column reads) | `collect()`, `schema()` and `explain()` raise `UrsaError`. Collect part of the chain and build the rest on its result. |
| A scan of a list of paths on different stores (for example a local file and an `s3://` file) | Put the files on one store, or read them into memory first. |

## Scope

- One machine, in memory. The graph index must fit in RAM.
- Node ids are `int64` or `string`. One frame uses one kind.
- There is no graph mutation. To change the graph, build a new frame.
- There is no visualization. Positions and drawing are out of scope for 0.4.

## Report a gap

If a limit blocks you, open an issue at [github.com/cldixon/ursa/issues](https://github.com/cldixon/ursa/issues) with the plan from `explain()`.
