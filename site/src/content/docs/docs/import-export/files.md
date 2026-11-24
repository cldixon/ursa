---
title: "Files"
description: "Read Parquet, CSV, JSON and Arrow files as tables, with read_parquet, read_csv and read_json, and write them with COPY."
---

Ursa reads files where they are: a file is a table. This page covers reading files by path and with `read_parquet`, `read_csv` and `read_json`, and writing them with `COPY … TO`. For files in S3, GCS or Azure, see [Object storage](/docs/import-export/object-storage/); for the formats the CLI prints, see [Output formats](/docs/import-export/output/).

The examples on this page share one session. They start with two views over the karate club files:

```sql
CREATE VIEW members AS SELECT * FROM 'karate_nodes.csv';
CREATE VIEW ties    AS SELECT * FROM 'karate_edges.csv';
```

## Reading and writing files

:::note[New in 0.7]
`.jsonl` and `.ndjson` files, `read_parquet`, `read_csv` and `read_json`, and `SHOW TABLES` without `information_schema` are new in `ursa-cli` 0.7. 0.6.0 reads `.parquet`, `.csv`, `.json` and `.arrow` files by path.
:::

A file is a table: name it in `FROM`, with its path or URL in quotes. A relative path is relative to the current directory. The format comes from the extension:

| Extension                        | Format                                    |
| -------------------------------- | ----------------------------------------- |
| `.parquet`                       | Parquet                                   |
| `.csv`                           | CSV, with a header line                   |
| `.json`, `.jsonl`, `.ndjson`     | JSON, one object per line (NDJSON)        |
| `.arrow`                         | Arrow IPC                                 |

CSV and JSON files may be compressed: `.csv.gz`, `.jsonl.zst`, and `.bz2` and `.xz` too. A directory or a glob (`'edges/*.parquet'`) reads every file in it with the extension of the first, and a `year=2024/` directory becomes a `year` column. `s3://`, `gs://`, `az://` and `https://` URLs work the same way.

`COPY … TO` writes a file, in the format its extension names, and prints how many rows it wrote:

```sql
COPY (SELECT * FROM ties) TO 'ties.jsonl';
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     78 │
└────────┘
```

```sql
SELECT count(*) AS ties FROM 'ties.jsonl';
```

```text
┌───────┐
│ ties  │
│ Int64 │
├───────┤
│    78 │
└───────┘
```

`STORED AS CSV` (or `PARQUET`, `JSON`, `ARROW`) names the format when the extension does not, and `OPTIONS (...)` takes DataFusion's writer options. DuckDB's option list works too: `COPY … TO 'ties.txt' (FORMAT csv, HEADER false, DELIMITER '|')`, with `FORMAT`, `HEADER`, `DELIMITER` (or `DELIM`, `SEP`) and `COMPRESSION`; see [Differences from DuckDB](/docs/duckdb/#what-is-different).

In the [Ursa Shell](https://ursa-shell.cl-dixon.workers.dev) in the browser, which has no disk, `COPY … TO 'ties.parquet'` keeps the file in the session: `FROM 'ties.parquet'` reads it and `.files` lists it, until the page is closed. The name has no folder, and `PARTITIONED BY` and compressed CSV or JSON are not written there.

:::tip[Convert large CSV files to Parquet once]
A view over a file reads the file each time a query uses it. For a CSV or JSON file, that means parsing all of it again, so a query that reads a large vertex table's attributes (a `JOIN` on its rows, `neighbors_agg`'s `attribute`, a `GRAPH_TABLE` property) pays for that every time. Parquet is much faster to read, and a query reads only the columns it needs. Convert the file once and define the view over the Parquet file:

```sql
COPY (FROM 'karate_nodes.csv') TO 'karate_nodes.parquet';
CREATE VIEW member_attributes AS SELECT * FROM 'karate_nodes.parquet';
```

This works for a compressed file too: `COPY (FROM 'users.csv.gz') TO 'users.parquet'`. A graph's edges are read once, into its [stored index](/docs/database/database-file/#stored-graph-indexes); the attributes of its vertex table are read from the file by each query that uses them.
:::

### read_parquet, read_csv and read_json

`read_parquet`, `read_csv` and `read_json` read a file as `FROM 'path'` does, but in the format they name, whatever the extension, and with options. They take a path, a glob, a directory, a URL, or a list of them:

```sql
COPY (SELECT * FROM ties) TO 'ties.txt' STORED AS CSV OPTIONS ('format.delimiter' '|');
SELECT count(*) AS ties FROM read_csv('ties.txt', delim => '|');
COPY (SELECT * FROM ties) TO 'ties.json';
SELECT count(*) AS both_files FROM read_json(['ties.jsonl', 'ties.json']);
```

```text
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     78 │
└────────┘
┌───────┐
│ ties  │
│ Int64 │
├───────┤
│    78 │
└───────┘
┌────────┐
│ count  │
│ UInt64 │
├────────┤
│     78 │
└────────┘
┌────────────┐
│ both_files │
│   Int64    │
├────────────┤
│        156 │
└────────────┘
```

| Function       | Options                                                                                                   |
| -------------- | --------------------------------------------------------------------------------------------------------- |
| `read_parquet` | none                                                                                                      |
| `read_csv`     | `header` (`true` or `false`), `delim` (one character; `sep` and `delimiter` are other names), `quote`, `compression` |
| `read_json`    | `compression`; `format`: `'newline_delimited'` (the default) or `'array'`, for a file that holds one JSON array of objects |

`compression` is `'auto'` (from the extension, the default), `'none'`, `'gzip'`, `'bzip2'`, `'xz'` or `'zstd'`. Options are named with `=>`, or with `=` as DuckDB writes them: `read_csv('ties.txt', header = false)`. `read_csv_auto` and `read_json_auto` are other names for `read_csv` and `read_json`, and `read_ndjson` is `read_json` for one object per line. A view over a `read_*` call, in a [database](/docs/database/database-file/), keeps its paths relative to the database file, as a view over `'path'` does.
