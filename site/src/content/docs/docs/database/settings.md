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
| `ursa.max_paths` | `10000000` | The most paths one `GRAPH_TABLE` path search holds before it stops with an error; `0` for no limit. Under `ANY SHORTEST` and `SHORTEST k` it counts the paths kept, never fewer than one (`k`) per node of the graph; otherwise every path and partial path held. See [pattern matching](/docs/graphs/pattern-matching/#how-many-paths). |
| `ursa.build_read_ahead` | `auto` | The most memory an index build holds in decoded rows read ahead of the ones it is numbering, so that the partitions of its source (and of a vertex table) decode on all cores at once. Bytes, or a number with `KiB`, `MiB`, `GiB` or `TiB`. `auto`: 256 MiB per partition of the source (one per core, as a rule), at most 1 GiB and at most an eighth of the memory available; less than 1 GiB slows a build of many partitions. `0` reads the source one partition at a time, holding nothing ahead: the least memory, the slowest build. The index is the same under any value. |

`SHOW ursa.index_storage_limit VERBOSE` adds a description; for the storage limit, it starts with where the limit comes from (`SET for this session`, or the default and the free space it is a share of). `SHOW ALL` lists every setting, DataFusion's and Ursa's. DataFusion's settings, such as `datafusion.execution.target_partitions`, work as in DataFusion. DuckDB's `SET threads = 4` sets `datafusion.execution.target_partitions`; any other name without an `ursa.` or `datafusion.` prefix is refused with a message that says so.

A setting lasts for the session, as in DuckDB and Postgres, in any database: `SET` and `RESET` write nothing to the database file. To keep a setting, put the `SET` in `~/.ursarc`, which the interactive shell runs at startup, or in a file you pass with `-init`, which scripts run too.
