# Errors

## When an error is raised

A frame checks each step as you add it. A step that cannot run on its frame raises on that line, before anything runs. Examples are a column the frame does not have, two columns with one name, a comparison between types that do not compare, a role such as `ur.id()` on a frame without one, a join key missing from either side, and a combination of steps that does not run yet. The exception class is the one `collect()` raised for it in earlier versions.

```py
nodes = ur.from_arrow({"id": [0, 1], "age": [31, 47]}, id="id")
try:
    nodes.filter(ur.col("agee") > 30)  # raises here, not at collect()
except ur.ColumnNotFoundError as exc:
    print(exc)  # filter() references unknown column 'agee'; the frame has ['id', 'age'].
```

Some errors depend on the rows or the files, so `collect()` still raises them: a null endpoint, a value out of range for its type, an integer divide by zero, an invalid value of a graph-verb parameter (`pagerank(damping=)`, `hop(n=)`), and a file that is missing or unreadable. A frame over `scan_edges` or `scan_nodes` is checked for the shape of its steps when you build it. Its columns are checked when it runs, because they come from the file, and the file can change in between.

## Engine errors

The engine raises one of three exception types. They are ordinary exceptions, so `except Exception` catches them.

| Type | Raised when |
|---|---|
| `ur.UrsaError` | The base class. Catch it to catch every engine error. |
| `ur.ColumnNotFoundError` | A step names a column that the frame does not have: a `filter`, `select`, `with_columns`, `rename`, `sort`, `group_by` or `agg`, a `join` key, a weight's edge column, a role column a scan's file lacks, or a role column an earlier step removed. See the shapes below. A subclass of both `UrsaError` and `ValueError`. |
| `ur.ComputeError` | A computation fails: an invalid parameter value, operand types that do not combine (`cannot compare string with int64 ...`), an integer division by zero, or a file a scan cannot read. A subclass of `UrsaError`. |

```py
try:
    result = nodes.with_columns(pr=ur.pagerank(edges)).collect()
except ur.ColumnNotFoundError as exc:
    print(exc)
```

## Error messages

A message names the step or the argument it is about, says what was wrong, and says what to do. It does not show the engine's internals: DataFusion's and Arrow's own wording is translated where it reaches you, and Ursa's private column names never appear.

| Shape | Example |
|---|---|
| A step and the column it names | `sort() references unknown column 'x'; the frame has ['id', 'age'].` |
| A call and the argument | `pagerank(max_iter=) must be a non-negative integer below 2**32; got -1` |
| A call and a value it does not take | `hop(direction='up') is not supported; use one of 'out', 'in', 'both'` |
| Operand types that do not combine | `cannot compare string with int64 (string > int64); compare values of one type` |
| A scan and its path | `scan_edges('edges.parquet'): no file matches the path; check it (a glob must match at least one file)` |

A `ColumnNotFoundError` names the step and the column, and lists the columns there are, in one of these shapes:

| Where | Message |
|---|---|
| A step's column | `sort() references unknown column 'x'; the frame has ['id', 'age'].` |
| A join key | `join(on=): key 'x' not in the left frame ['id', 'age'].` (or the right frame) |
| A weight | `weight expression references unknown edge column(s): ['x']; the edges have ['s', 'd']` |
| A scan's role column | `scan_edges('edges.parquet'): src column 'x' not found in the file; it has ['s', 'd']` |
| A role an earlier step removed | `a graph op needs the edge frame's src column 's', but select() removed it, ...` |
| A step's column on a scan, checked at `collect()` | `filter() references unknown column 'x'; the frame has ['id', 'age'].` |

When Ursa translates an engine error, the engine's original text stays on the exception as its `__cause__`, a `RuntimeError`, so a bug report can include both:

```python
import ursa as ur

nodes = ur.from_arrow({"id": [0, 1], "age": [31, 47]}, id="id")
try:
    nodes.with_columns(per_zero=ur.col("age") / 0).collect()
except ur.ComputeError as exc:
    print(exc)  # division by zero in an integer expression; divide by a float ...
    print(exc.__cause__)  # Arrow error: Divide by zero error
```

The messages are part of the API: a change to one is listed in the changelog.

## Other exceptions

| Type | Raised when |
|---|---|
| `NotImplementedError` | The plan has a step, or a combination of steps, that does not run yet. Raised by the step that adds it. See [Limits](../limits.md). |
| `ValueError` | An argument is invalid: an unknown `on_null` value, an edge tuple of the wrong length, two aggregations with one name, a `select` or `rename` that would give two columns one name, or a string argument given another type (`join(how=) must be a string; got 1`). Raised by the step that takes it. A `ColumnNotFoundError` is a `ValueError` too. |
| `TypeError`, `OverflowError` | An integer argument is not an integer (`hop(n=) must be an integer; got a str`) or is out of range (`random_walk(steps=) must be a non-negative integer; got -1`). A column role is not a name (`src= must be a column name (a str); got 1 (int)`). A scan path is not a string or a `pathlib.Path`, or a scan gets an unknown keyword (`scan_edges() got an unexpected keyword argument 'sep'; use delimiter= instead.`). `join(other)` gets a value that is not a frame. A frame cannot be pickled or deep-copied. A graph function gets something other than an edge frame (see [below](#a-graph-function-given-something-other-than-an-edge-frame)). `bool(expr)`, and so `and`, `or`, `not` and `if` on an expression, raises `TypeError: the truth value of an ursa expression is ambiguous ...`. |
| `KeyError` | A role column (`src=`, `dst=`, `id=`) is not in the table: `src column 'source' is not in the table; it has ['s', 'd']`. |
| `AttributeError` | A value that is no expression where one is needed (`filter() takes an ursa expression ...`). |
| `ModuleNotFoundError` | `to_polars()` is called without `polars` installed. The message names the `ursa-graph[polars]` extra. |
| `RuntimeError` | A dataset download fails. See [Datasets](datasets.md#the-download-cache). |

## A graph function given something other than an edge frame

Each function that takes `edges` (the algorithms, `neighbors`, `hop`, `shortest_path`, `random_walk`, and the statistics) checks it where you call it. A node frame, a graph verb, a `describe` or `random_walk` result, a collected result, a table or any other value raises `TypeError`. The message names the function, says what it got, and says what to pass:

```py
edges = ur.from_arrow({"s": [0, 1], "d": [1, 0]}, src="s", dst="d")
try:
    ur.density(ur.degree(edges))
except TypeError as exc:
    print(exc)
    # density() takes an edge frame (from ur.from_arrow(..., src=, dst=) or ur.scan_edges);
    # got a node frame (ur.degree(...)); pass the edges the verb was built from.
```

An edge frame of any kind passes the check: a filtered or reshaped frame, `edges.reverse()`, and a `hop` or `shortest_path` result.

## Warnings

`on_null="drop"` emits a `UserWarning` with the number of rows it removed. Use the `warnings` module to silence it or to turn it into an error.
