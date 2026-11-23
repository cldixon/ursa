---
title: "Differences from DuckDB"
description: "What carries over from DuckDB to Ursa's SQL and shell, and what does not."
---

:::note[New in 0.7]
`read_parquet`, `read_csv` and `read_json`, `.jsonl` and `.ndjson` files, and `SHOW TABLES` without `information_schema` are new in `ursa-cli` 0.7. The rest of this page holds for 0.6.0 too.
:::

Ursa's SQL works like DuckDB's in the ways you use most: you name a file in `FROM`, and the shell takes the same flags and dot-commands. The engine underneath is [Apache DataFusion](https://datafusion.apache.org/), though, not DuckDB. This page lists what carries over and what does not.

## What works the same

**Files are tables.** Name a file, a glob, a directory or a URL in `FROM`, in quotes:

```sql
SELECT count(*) AS ties FROM 'karate_edges.csv';
```

```text
┌───────┐
│ ties  │
│ Int64 │
├───────┤
│    78 │
└───────┘
```

Without the quotes, `FROM karate_edges.csv` names a table `csv` in a schema `karate_edges`; the error that it is not found says to quote the path. `SHOW 'karate_edges.csv'` is `DESCRIBE 'karate_edges.csv'`, as in DuckDB.

**`read_parquet`, `read_csv` and `read_json`** read the same files, with a few of DuckDB's options. DuckDB's `name = value` form works, as does Ursa's `name => value`:

```sql
SELECT * FROM read_csv('karate_edges.csv', header = true, delim = ',') LIMIT 2;
```

```text
┌───────┬───────┐
│  src  │  dst  │
│ Int64 │ Int64 │
├───────┼───────┤
│     0 │     1 │
│     0 │     2 │
└───────┴───────┘
```

[Reading and writing files](/docs/import-export/files/) lists the options. `read_csv_auto`, `read_json_auto` and `read_ndjson` are other names for the same functions.

**FROM-first queries**, `EXCLUDE`, `REPLACE`, `GROUP BY ALL`, `ORDER BY ALL` and `QUALIFY`:

```sql
FROM 'karate_edges.csv' LIMIT 1;
SELECT * EXCLUDE (dst) FROM 'karate_edges.csv' LIMIT 1;
SELECT src, count(*) AS friends FROM 'karate_edges.csv' GROUP BY ALL ORDER BY friends DESC LIMIT 1;
```

A FROM-first query works wherever a query does: in a subquery, in `CREATE VIEW v AS FROM t`, in `COPY (FROM t) TO …`, and after `EXPLAIN` and `DESCRIBE` (`DESCRIBE FROM 'karate_edges.csv'` lists the file's columns). `ORDER BY ALL` orders by every column of the `SELECT` list, left to right, where the list names its columns; after `SELECT *`, name the columns or their positions (`ORDER BY 1, 2`).

```text
┌───────┬───────┐
│  src  │  dst  │
│ Int64 │ Int64 │
├───────┼───────┤
│     0 │     1 │
└───────┴───────┘
┌───────┐
│  src  │
│ Int64 │
├───────┤
│     0 │
└───────┘
┌───────┬─────────┐
│  src  │ friends │
│ Int64 │  Int64  │
├───────┼─────────┤
│     0 │      16 │
└───────┴─────────┘
```

**`DESCRIBE`** takes a table, a file or a query:

```sql
DESCRIBE 'karate_edges.csv';
```

```text
┌─────────────┬───────────┬─────────────┐
│ column_name │ data_type │ is_nullable │
│    Utf8     │   Utf8    │    Utf8     │
├─────────────┼───────────┼─────────────┤
│ src         │ Int64     │ YES         │
│ dst         │ Int64     │ YES         │
└─────────────┴───────────┴─────────────┘
```

**The shell.** `ursa club.ursa` opens or creates a database file, as `duckdb club.duckdb` does. `-c` runs SQL and exits, and `-f` runs a file. The dot-commands include `.tables`, `.schema`, `.mode` (with DuckDB's `duckbox`, `markdown`, `line` and `jsonlines`), `.headers`, `.output`, `.once`, `.read`, `.timer` and `.help`; `.nullvalue`, `.separator`, `.databases` and `.shell` are not there. See [the shell](/docs/cli/shell/).

**Graphs.** `CREATE PROPERTY GRAPH` and `GRAPH_TABLE` follow SQL/PGQ, as DuckDB's DuckPGQ extension does, and DuckPGQ's examples run as written. See [pattern matching](/docs/graphs/pattern-matching/). DuckPGQ's `DROP PROPERTY GRAPH` and `SHOW PROPERTY GRAPHS` work, and may leave out `PROPERTY` (`DROP GRAPH g`, `SHOW GRAPHS`). `SUMMARIZE PROPERTY GRAPH g` returns `describe(g)`'s row for a graph of one edge table; for more, call `describe(g, vertex, edge)` with the pair. `ALTER PROPERTY GRAPH` is not supported: write the whole new definition with `CREATE OR REPLACE PROPERTY GRAPH`.

**Settings and temporary tables.** `SET threads = 4` sets `datafusion.execution.target_partitions`, the number of partitions a query runs in. Ursa's other settings start with `ursa.` or `datafusion.`, and `SHOW ALL` lists them; DuckDB's `memory_limit` is `datafusion.runtime.memory_limit`. `CREATE TEMP TABLE` and `CREATE TEMP VIEW` work. In a database file they are not stored.

## What is different

**Column names.** An expression without an alias is named the way DataFusion writes it, with its types: `SELECT 42` gives a column named `Int64(42)`, where DuckDB says `42`. Name the columns you keep with `AS`:

```sql
SELECT 42 AS answer;
```

```text
┌────────┐
│ answer │
│ Int64  │
├────────┤
│     42 │
└────────┘
```

**The catalog.** The default catalog is `datafusion` and the default schema is `public`. DuckDB uses `memory` (or the database's name) and `main`. `SHOW TABLES` returns four columns: `table_catalog`, `table_schema`, `table_name` and `table_type`. A graph's `nodes` and `edges` tables are listed under the graph's name.

**JSON.** A `.json` file is read as newline-delimited JSON, one object per line, as are `.jsonl` and `.ndjson`. DuckDB detects the layout. For a file that holds one JSON array of objects, use `read_json(path, format => 'array')`.

**CSV.** Ursa assumes the first line is a header and the delimiter is a comma, unless `header` or `delim` says otherwise. DuckDB detects both. Column types are inferred from the data in both.

**`COPY`.** `COPY … TO` prints one row, the number of rows written:

```sql
COPY (SELECT * FROM 'karate_edges.csv') TO 'ties.parquet';
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     78 │
└────────┘
```

The format comes from the extension, or from `FORMAT` in DuckDB's option list, which Ursa maps onto DataFusion's `STORED AS` and `OPTIONS (...)`:

```sql
COPY (FROM 'karate_edges.csv') TO 'ties.txt' (FORMAT csv, HEADER false, DELIMITER '|');
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     78 │
└────────┘
```

| DuckDB | DataFusion |
| --- | --- |
| `FORMAT parquet` | `STORED AS PARQUET` |
| `HEADER`, `HEADER false` | `OPTIONS ('format.has_header' 'true')` |
| `DELIMITER '\|'`, `DELIM`, `SEP` | `OPTIONS ('format.delimiter' '\|')` |
| `COMPRESSION zstd` | `OPTIONS ('format.compression' 'zstd(3)')` for Parquet; `zstd`, `gzip` and `brotli` get a level |

`WITH (...)` works too. Any other option, such as `ROW_GROUP_SIZE` or `PARTITION_BY`, is refused by name; DataFusion's own options go in `OPTIONS (...)`.

**Division.** In DataFusion, `/` between two integers is integer division, rounded toward zero, as in PostgreSQL: `7 / 2` is `3`, and `count(*) / 3` is an integer. DuckDB's `/` returns a `DOUBLE` (3.5), and DuckDB's `//` (integer division) is not supported. With a `DECIMAL` (`NUMERIC`) on either side, the result is a `DECIMAL`. For a float, make one side a float, with `CAST(a AS DOUBLE)` or `* 1.0`; for an integer from a float or a decimal, `CAST(… AS BIGINT)` rounds toward zero:

```sql
SELECT 7 / 2 AS integers,
       CAST(7 AS DOUBLE) / 2 AS doubles,
       7 * 1.0 / 2 AS also_doubles,
       CAST(7 AS DECIMAL(10, 2)) / 2 AS decimals,
       CAST(7.9 AS BIGINT) AS truncated;
```

```text
┌──────────┬─────────┬──────────────┬───────────────────┬───────────┐
│ integers │ doubles │ also_doubles │     decimals      │ truncated │
│  Int64   │ Float64 │   Float64    │ Decimal128(14, 6) │   Int64   │
├──────────┼─────────┼──────────────┼───────────────────┼───────────┤
│        3 │     3.5 │          3.5 │          3.500000 │         7 │
└──────────┴─────────┴──────────────┴───────────────────┴───────────┘
```

**Functions.** Scalar and aggregate functions are DataFusion's. Most common ones share DuckDB's names, and Ursa adds DuckDB's names for a few that DataFusion calls something else:

| DuckDB | Runs as |
| --- | --- |
| `list_value(a, b, …)` | `make_array(a, b, …)` |
| `strftime(t, format)` | `to_char(t, format)`; `strftime(format, t)` works too |
| `epoch_ms(n)` | the timestamp `n` milliseconds after 1970-01-01 |
| `epoch_ms(t)` | the milliseconds from 1970-01-01 to `t`, a `BIGINT` |
| `arg_max(x, y)`, `arg_min(x, y)` | `first_value(x ORDER BY y DESC)` (`ASC`), skipping rows where `x` or `y` is NULL |

```sql
SELECT list_value(1, 2) AS l,
       strftime(DATE '2024-03-05', '%d/%m/%Y') AS day,
       epoch_ms(86400000) AS t,
       arg_max(dst, src) AS last_dst
FROM 'karate_edges.csv';
```

```text
placeholder
```

`arg_max` and `arg_min` are aggregates, not window functions. DuckDB's lambdas (`list_transform(l, x -> x + 1)`) and reuse of a column alias in the same `SELECT` are not supported. The graph functions are table functions: `SELECT pagerank(g)` says to write `SELECT * FROM pagerank(g)`, and a graph function named as a column (`SELECT id, pagerank FROM g.nodes`) says to join its rows on `id`. `SELECT * FROM ursa_functions()` lists the graph functions, and `SHOW FUNCTIONS` lists the rest.

**Not supported:**

- `SUMMARIZE` of a table or query;
- `USING SAMPLE` and `TABLESAMPLE`. Both are refused with an error; for a sample of n rows, use `ORDER BY random() LIMIT n`;
- `PRAGMA`, `INSTALL` and `LOAD`. Ursa has no extensions: object storage, Parquet, CSV and JSON are built in;
- `ATTACH … AS`. A session has one database.
