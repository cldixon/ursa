---
title: "Output formats"
description: "Print results as tables, CSV, TSV, JSON, NDJSON or Arrow, for people, files and programs."
---

## Output modes

The shell prints `table` by default. `.mode csv`, `.mode tsv`, `.mode json` and `.mode ndjson` switch it. On the command line, `--format` sets the mode for the shell and for `-c` alike:

```bash
ursa club.ursa --format json -c "SELECT * FROM degree(karate) ORDER BY id LIMIT 2"
```

```text
[{"id":0,"degree":16},{"id":1,"degree":8}]
```

`--format arrow` writes an Arrow IPC stream, for another program to read. It works only with `-c` or `-f`, and only for one query that returns rows: a second result, or none at all (a script of only `CREATE` statements), is an error. `.mode` cannot switch to it.

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

`csv`, `tsv`, `json`, `ndjson` and `arrow` write every row and never cut a value. A `table` shows at most 40 rows (see [Long results](/docs/cli/shell/#long-results)).
