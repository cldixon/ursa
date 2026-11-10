# Differences from DuckDB

!!! info "New in 0.7"

    `read_parquet`, `read_csv` and `read_json`, `.jsonl` and `.ndjson` files, and `SHOW TABLES` without `information_schema` are new in `ursa-cli` 0.7. The rest of this page holds for 0.6.0 too.

Ursa's SQL works like DuckDB's in the ways you use most: you name a file in `FROM`, and the shell takes the same flags and dot-commands. The engine underneath is [Apache DataFusion](https://datafusion.apache.org/), though, not DuckDB. This page lists what carries over and what does not.

## What works the same

**Files are tables.** Name a file, a glob, a directory or a URL in `FROM`:

```sql
SELECT count(*) AS ties FROM 'karate_edges.csv';
```

```text
┌───────┐
│ ties  │
│ int64 │
├───────┤
│    78 │
└───────┘
```

**`read_parquet`, `read_csv` and `read_json`** read the same files, with a few of DuckDB's options. DuckDB's `name = value` form works, as does Ursa's `name => value`:

```sql
SELECT * FROM read_csv('karate_edges.csv', header = true, delim = ',') LIMIT 2;
```

```text
┌───────┬───────┐
│  src  │  dst  │
│ int64 │ int64 │
├───────┼───────┤
│     0 │     1 │
│     0 │     2 │
└───────┴───────┘
```

[Reading and writing files](statements.md#reading-and-writing-files) lists the options. `read_csv_auto`, `read_json_auto` and `read_ndjson` are other names for the same functions.

**FROM-first queries**, `EXCLUDE`, `REPLACE`, `GROUP BY ALL` and `QUALIFY`:

```sql
FROM 'karate_edges.csv' LIMIT 1;
SELECT * EXCLUDE (dst) FROM 'karate_edges.csv' LIMIT 1;
SELECT src, count(*) AS friends FROM 'karate_edges.csv' GROUP BY ALL ORDER BY friends DESC LIMIT 1;
```

```text
┌───────┬───────┐
│  src  │  dst  │
│ int64 │ int64 │
├───────┼───────┤
│     0 │     1 │
└───────┴───────┘
┌───────┐
│  src  │
│ int64 │
├───────┤
│     0 │
└───────┘
┌───────┬─────────┐
│  src  │ friends │
│ int64 │  int64  │
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
│   varchar   │  varchar  │   varchar   │
├─────────────┼───────────┼─────────────┤
│ src         │ Int64     │ YES         │
│ dst         │ Int64     │ YES         │
└─────────────┴───────────┴─────────────┘
```

**The shell.** `ursa club.ursa` opens or creates a database file, as `duckdb club.duckdb` does. `-c` runs SQL and exits, and `-f` runs a file. The dot-commands include `.tables`, `.mode`, `.read`, `.timer` and `.help`. See [the shell](shell.md).

**Graphs.** `CREATE PROPERTY GRAPH` and `GRAPH_TABLE` follow SQL/PGQ, as DuckDB's DuckPGQ extension does, and DuckPGQ's examples run as written. See [pattern matching](pattern-matching.md).

## What is different

**Column names.** An expression without an alias is named the way DataFusion writes it, with its types: `SELECT 42` gives a column named `Int64(42)`, where DuckDB says `42`. Name the columns you keep with `AS`:

```sql
SELECT 42 AS answer;
```

```text
┌────────┐
│ answer │
│ int64  │
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
│ uint64 │
├────────┤
│     78 │
└────────┘
```

The format comes from the extension, or from `STORED AS PARQUET`. Options go in `OPTIONS (...)`, not DuckDB's `(FORMAT parquet, ...)`.

**Division.** In DataFusion, `/` between two integers is integer division, rounded toward zero, as in PostgreSQL: `7 / 2` is `3`, and `count(*) / 3` is an integer. DuckDB's `/` returns a `DOUBLE` (3.5), and DuckDB's `//` (integer division) is not supported. With a `DECIMAL` (`NUMERIC`) on either side, the result is a `DECIMAL`. For a float, make one side a float, with `CAST(a AS DOUBLE)` or `* 1.0`; for an integer from a float or a decimal, `CAST(… AS BIGINT)` rounds toward zero:

```sql
SELECT 7 / 2 AS integers,
       CAST(7 AS DOUBLE) / 2 AS doubles,
       7 * 1.0 / 2 AS also_doubles,
       CAST(7 AS DECIMAL(10, 2)) / 2 AS decimals,
       CAST(7.9 AS BIGINT) AS truncated;
```

```text
┌──────────┬─────────┬──────────────┬───────────────┬───────────┐
│ integers │ doubles │ also_doubles │   decimals    │ truncated │
│  int64   │ double  │    double    │ decimal(14,6) │   int64   │
├──────────┼─────────┼──────────────┼───────────────┼───────────┤
│        3 │     3.5 │          3.5 │      3.500000 │         7 │
└──────────┴─────────┴──────────────┴───────────────┴───────────┘
```

**Functions.** Scalar and aggregate functions are DataFusion's. Most common ones share DuckDB's names, but some do not: `to_char` formats a date where DuckDB has `strftime`, and `make_array` builds a list where DuckDB also has `list_value`. `SELECT * FROM ursa_functions()` lists the graph functions, and `SHOW FUNCTIONS` lists the rest.

**Not supported:**

- `SUMMARIZE`;
- `USING SAMPLE` and `TABLESAMPLE`. Both are refused with an error; for a sample of n rows, use `ORDER BY random() LIMIT n`;
- `ORDER BY ALL`;
- `PRAGMA`, `INSTALL` and `LOAD`. Ursa has no extensions: object storage, Parquet, CSV and JSON are built in;
- `ATTACH … AS`. A session has one database.
