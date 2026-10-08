---
icon: lucide/book-open
---

# Reference

Every public name in `ursa`, grouped by role. The import convention on every page is `import ursa as ur`.

| Page | Contains |
|---|---|
| [Frames](frames.md) | `EdgeFrame`, `NodeFrame`, `MaterializedFrame`, and the methods they share |
| [Input and output](io.md) | Constructors, scans, and interop with polars, pandas, NetworkX, NumPy, and SciPy |
| [Algorithms](algorithms.md) | The graph kernels, neighbour aggregation, and the traversals |
| [Expressions](expressions.md) | `col`, `lit`, `src`, `dst`, `id`, operators, and aggregations |
| [Statistics](statistics.md) | `describe`, `density`, `avg_path_length`, `diameter` |
| [Datasets](datasets.md) | The bundled and downloadable example graphs |
| [Errors](errors.md) | The exception types |

## Conventions

- A parameter after `*` is keyword-only.
- "Lazy" means the call builds a plan and returns at once. `collect()` runs the plan.
- "Eager" means the call runs at once and returns a plain Python value.
- Node ids are `int64` or `string`. Ursa detects the kind from the column type. A frame uses one kind.

## Version

The page describes `ursa-graph` 0.3. `ur.__version__` gives the installed version.
