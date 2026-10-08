---
icon: lucide/compass
---

# Guides

Each guide shows one task, with code that runs and the output it gives. The guides use the bundled datasets, so you can paste the code into a Python session with no files and no network.

| Guide | Task |
|---|---|
| [Load data](load-data.md) | Build a frame from memory, from a file, from object storage, or from another library. |
| [Compute graph metrics](metrics.md) | Centrality, components, triangles, and communities, as columns. |
| [Use node attributes](attributes.md) | Join your own node table, filter on it, and aggregate over neighbours. |
| [Traverse the graph](traversals.md) | Hops, shortest paths, and random walks. |
| [Weight the edges](weights.md) | Run a weighted algorithm with an expression over edge columns. |
| [Work on a subgraph](subgraphs.md) | Run algorithms over a filtered edge set or a traversal result. |
| [Shape the result](relational.md) | Filter, sort, select, group, and join. |
| [Summarize the graph](statistics.md) | Density, path length, diameter, and `describe`. |
| [Get the result out](output.md) | Arrow, polars, dicts, Parquet, and CSV. |

Every guide starts with the same two lines:

```py
import ursa as ur

edges = ur.datasets.load_karate()
```
