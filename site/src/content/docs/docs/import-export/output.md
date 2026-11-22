---
title: "Output formats"
description: "Print results as tables, CSV, TSV, JSON, NDJSON or Arrow, for people, files and programs."
---

## Output modes

The shell prints `table` by default. `.mode csv`, `.mode tsv`, `.mode json`, `.mode ndjson`, `.mode markdown` and `.mode line` switch it. DuckDB's names for the same modes work too: `duckbox` and `box` are `table`, `jsonlines` is `ndjson`, and `lines` is `line`. On the command line, `--format` sets the mode for the shell and for `-c` alike:

```bash
ursa club.ursa --format json -c "SELECT * FROM degree(karate) ORDER BY id LIMIT 2"
```

```text
[{"id":0,"degree":16},{"id":1,"degree":8}]
```

`--format arrow` writes an Arrow IPC stream, for another program to read. It works only with `-c` or `-f`, and only for one query that returns rows: a second result, or none at all (a script of only `CREATE` statements), is an error. `.mode` cannot switch to it.

`markdown` writes a Markdown table, as DuckDB's `.mode markdown` does, to paste into an issue, a document or a chat. Each name is centred over its column, a number column's separator ends in `:` so it is right-aligned where the table is rendered, and a null is `NULL`. A `|` in a value is written `\|` and a line break `\n`, so each row stays one row of the table:

```sql
.mode markdown
SELECT id, club FROM 'karate_nodes.csv' LIMIT 2;
```

```text
| id |  club  |
|---:|--------|
| 0  | Mr. Hi |
| 1  | Mr. Hi |
```

`line` writes one `column = value` line per column, the names right-aligned, and a blank line between rows, as DuckDB's `.mode line` does. It is the readable mode for a wide row:

```sql
.mode line
SELECT name, family, networkx FROM ursa_functions() WHERE name = 'pagerank';
```

```text
    name = pagerank
  family = algorithm
networkx = nx.pagerank(G, alpha=damping) (both: nx.pagerank(U))
```

`.headers off` leaves out the header row of `csv` and `tsv`, as in DuckDB; `.headers on` puts it back. The other modes always name their columns.

### Reading results in a program

Read results in a program, or as an agent, as `json` or `ndjson`. Every row object has every column, in column order, with `null` for a null, as DuckDB writes them. A list, such as `vertices(p)` or `array_agg(x)`, is a JSON array, and a struct a JSON object:

```bash
ursa --format ndjson -c "SELECT * FROM (VALUES (1, NULL, [1, 2]), (2, 'x', [3])) t(n, s, l)"
```

```text
{"n":1,"s":null,"l":[1,2]}
{"n":2,"s":"x","l":[3]}
```

`csv` and `tsv` are for files and spreadsheets. They write as DuckDB does: a list, struct or map is its text form, quoted where CSV needs it; a null is an empty field and an empty string is `""`, so the two can be told apart:

```bash
ursa --format csv -c "SELECT NULL AS a, '' AS b, [1, 2] AS l"
```

```text
a,b,l
,"","[1, 2]"
```

A string inside a list, struct or map is in single quotes where it needs them, as in DuckDB: when it is empty, is `NULL` in any case, starts or ends with a space, or holds any of `' " \ , : = [ ] { } ( )`. A `'` or `\` inside the quotes has a `\` before it. A null inside is a bare `NULL`, so it cannot be mistaken for the string `'NULL'`:

```bash
ursa --format csv -c "SELECT ['a, b', 'c'] AS l, ['x]', NULL, 'NULL'] AS m"
```

```text
l,m
"['a, b', c]","['x]', NULL, 'NULL']"
```

The table writes nested values the same way. This text is for reading: JSON is the lossless format for nested results, since it keeps each value's type, so read lists, structs and maps (such as `vertices(p)` on a graph with string ids) as `json` or `ndjson`.

`csv`, `tsv`, `json`, `ndjson` and `arrow` write every row and never cut a value. A `table` shows at most 40 rows (see [Long results](/docs/cli/shell/#long-results)).
