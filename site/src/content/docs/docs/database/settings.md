---
title: "Settings"
description: "Ursa's settings, set with SET and read with SHOW."
---

Ursa's settings start with `ursa.`. Set one with `SET` and read it with `SHOW`:

```sql
SET ursa.index_storage_limit = '8GiB';
SHOW ursa.index_storage_limit;
```

```text
┌──────────────────────────┬───────┐
│           name           │ value │
│           Utf8           │ Utf8  │
├──────────────────────────┼───────┤
│ ursa.index_storage_limit │ 8GiB  │
└──────────────────────────┴───────┘
```

| Setting                    | Default | Meaning                                                                                                                                         |
| -------------------------- | ------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `ursa.index_storage_limit` | `auto`  | The most space stored graph indexes may take in a database. Bytes, or a number with `KiB`, `MiB`, `GiB` or `TiB`. `auto`: 25% of the free space on the database's disk, rounded down to a multiple of 4 GiB, at least 4 GiB and at most 64 GiB. `SHOW` says the limit in effect; `SHOW … VERBOSE` says where it comes from. See [the database file](/docs/database/database-file/#the-storage-limit). |
| `ursa.max_paths` | `10000000` | The most paths one `GRAPH_TABLE` path search holds before it stops with an error; `0` for no limit. Under `ANY SHORTEST` and `SHORTEST k` it counts the paths kept, never fewer than one (`k`) per node of the graph; otherwise every path and partial path held. A value below that floor is used as the floor, with a notice. A value that is not a whole number is refused: `ursa.max_paths must be a non-negative integer (0 for no limit); got -5`. See [pattern matching](/docs/graphs/pattern-matching/#how-many-paths). |
| `ursa.build_read_ahead` | `auto` | The most memory an index build holds in decoded rows read ahead of the ones it is numbering, so that the partitions of its source (and of a vertex table) decode on all cores at once. Bytes, or a number with `KiB`, `MiB`, `GiB` or `TiB`. `auto`: 256 MiB per partition of the source (one per core, as a rule), at most 1 GiB and at most an eighth of the memory available; less than 1 GiB slows a build of many partitions. `0` reads the source one partition at a time, holding nothing ahead: the least memory, the slowest build. The index is the same under any value. |
| `ursa.graph_max_edges` | `5000000` | The most edges the graph view draws (`.graph` in the web shell): a result or a graph with more is refused, with a message that names this setting; `0` for no limit. The default is what a desktop browser draws; the web shell starts a session on a phone (a coarse pointer and at most 4 GB of memory) at `500000`. Past about two million edges a layout takes a minute or so, so compute it once and draw from it: `COPY (SELECT * FROM layout_fa2(g)) TO 'pos.parquet'`, then `.graph g POSITIONS (SELECT * FROM 'pos.parquet')`. |
| `ursa.index_layout` | `plain` | How a new graph index lays out its adjacency. `plain`: each neighbour a 4-byte id, the fastest to read. `compressed`: each node's neighbours sorted and stored as varint-coded gaps, about 1.6 to 1.7 bytes a neighbour on a large graph against 4 (both directions, and a stored index on disk too), for slower algorithms: most within 10% of the plain layout's time, weakly connected components about 1.5 times it. An index is cached and stored under its layout, so after a `SET` the next query builds (or loads) the index in the new layout, and the old one is still there to go back to. Results are the same in either, but for the last bits of sums in neighbour order (PageRank, betweenness) and which of several equally short paths `shortest_path` returns. See [compressed indexes](#compressed-indexes). |

`SHOW ursa.index_storage_limit VERBOSE` adds a description; for the storage limit, it starts with where the limit comes from (`SET for this session`, or the default and the free space it is a share of). `SHOW ALL` lists every setting, DataFusion's and Ursa's. DataFusion's settings, such as `datafusion.execution.target_partitions`, work as in DataFusion. DuckDB's `SET threads = 4` sets `datafusion.execution.target_partitions`; any other name without an `ursa.` or `datafusion.` prefix is refused with a message that says so.

A setting lasts for the session, as in DuckDB and Postgres, in any database: `SET` and `RESET` write nothing to the database file. To keep a setting, put the `SET` in `~/.ursarc`, which the interactive shell runs at startup, or in a file you pass with `-init`, which scripts run too.

### Compressed indexes

`SET ursa.index_layout = 'compressed'` builds a graph's index in the compressed layout (#173): each node's neighbours are sorted, and stored as the gaps between them, each gap a varint of one byte when it is below 128, which it mostly is once the nodes are numbered by in-degree. On R-MAT graphs of 16 and 64 million edges the neighbours take 1.56 to 1.71 bytes an edge instead of 4, and an adjacency with its offsets 2.6 to 2.8 instead of 4.5, in each direction. A node's degree and its edge ids are read without decoding. Edge ids, which only a weight, a mask or an edge's row reads, follow the sorted order and are built only when a query reads them, as in the plain layout.

The price is the decoding: an algorithm reads each neighbour list a byte at a time, and the build sorts and encodes it, about 0.9 s more at 64 million edges. On R-MAT graphs of 16 and 64 million edges, PageRank, strong components, `hop`, BFS, triangles, Louvain and betweenness take between 0.85 and 1.1 times the plain layout's time, label propagation and closeness up to 1.2 times, and weakly connected components 1.5 times; see the changelog for the measurements. A path search (`GRAPH_TABLE` with a quantifier) puts each node's edges back in edge-row order as it extends a path, so it finds the same paths as on a plain index.

Every result is the plain layout's, but for two things that follow the order of a node's neighbours, which the compressed layout sorts: sums of floating-point values (PageRank, betweenness) may differ in their last bits, and among several shortest paths of one length `shortest_path` may return another.
