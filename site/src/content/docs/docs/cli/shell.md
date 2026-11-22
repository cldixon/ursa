---
title: "The shell"
description: "Ursa's interactive shell: the prompt, dot-commands, how tables are drawn, init files and history."
---

`ursa` with no SQL to run starts an interactive shell. Give it a database file to keep what you define, or nothing to work in memory:

```bash
ursa club.ursa
ursa
```

```text
ursa 0.11.0, database club.ursa. Enter .help for commands, .quit to exit.
club U
```

Type SQL at the prompt. A statement ends with `;` and may span lines. Results print as tables. An error prints a message and the session goes on.

The prompt names the database, as DuckDB's does: `memory U` in a session without one, `club U` with `club.ursa` open. While a statement is unfinished its next lines are indented as far as the prompt, so a statement copied from the terminal is still SQL:

```text
memory U SELECT id, degree
         FROM degree(karate)
         LIMIT 3;
```

| Key      | Does                                       |
| -------- | ------------------------------------------ |
| Ctrl-C   | Cancels the line being typed, or the query |
| Ctrl-D   | Exits                                      |
| Up, Down | Steps through history                      |

To run SQL from scripts and other programs, see [Command line](/docs/cli/command-line/).

## Dot-commands

A line that starts with `.` is a shell command, not SQL. The commands follow DuckDB's shell where it has the same one. A `;` at the end of a command is ignored (`.tables;`), except by `.print`, which prints its text as it is, but with control characters other than tabs written as escapes (ESC as `\u{1b}`, a carriage return as `\r`), as tables write them, so printed text cannot move the cursor or change the terminal's colours. `.help PATTERN` lists the commands whose name or description has PATTERN in it; `.help graph` is a page on the graph SQL (defining a graph, `MATCH` patterns and algorithms) and `.help shortcuts` lists the keys.

```text
memory U .help
.describe NAME       Describe a graph, table or view
.exit                Exit the shell (also Ctrl-D)
.functions ?NAME?    List the graph functions, or show one's parameters
.graphs              List the graphs (SHOW GRAPHS)
.headers on|off      Turn the header row of csv and tsv output on or off
.help ?PATTERN?      Show help for the commands matching PATTERN
.maxrows N           Show at most N rows of a table (default 40; 0 shows all)
.maxwidth N          Draw tables at most N characters wide (0: the screen's width)
.mode MODE           Set the output mode: table, csv, tsv, json, ndjson, markdown or line
.once FILE           Send the output of the next command only to FILE
.open FILE.ursa      Open a database file, before anything is defined (as ATTACH)
.output ?FILE?       Send output to FILE, or back to the screen if FILE is omitted
.print STRING...     Print literal STRING
.quit                Exit the shell (also Ctrl-D)
.read FILE           Run the SQL and dot-commands in FILE
.schema ?PATTERN?    Show the statements that define the views, tables and graphs
.tables              List the tables and views
.timer on|off        Show how long each statement takes (real, user and sys)
.version             Show the version

DuckDB's .mode names duckbox, box and jsonlines work too
Run .help graph for the graph SQL: property graphs, MATCH and algorithms
Run .help shortcuts for keyboard shortcuts
```

| Command           | Same as                                    |
| ----------------- | ------------------------------------------ |
| `.graphs`         | `SHOW GRAPHS;`                             |
| `.describe karate` | `DESCRIBE GRAPH karate;` for a graph, `DESCRIBE t;` for a table or view |
| `.functions`      | `SELECT name, family, summary FROM ursa_functions();`, with the first sentence of each summary |
| `.open club.ursa` | `ATTACH 'club.ursa';`                      |
| `.tables`         | a query on `information_schema.tables`, without the graphs' own tables |
| `.schema`         | the statements in the database's catalog, each ending with `;` |

`.functions NAME` shows one function: its summary, then its parameters one a line (name, type, default, what it does, and the values it takes), then its output columns:

```text
memory U .functions degree
degree (algorithm)
Per-node degree.
NetworkX: G.out_degree() (in: G.in_degree(); both: U.degree() on a simple graph)

Parameters, after the graph, as name => value:
  direction  string  default 'out'  Follow out-edges, in-edges, or both (the undirected view). One of: out, in, both.

Output columns:
  degree  UInt32
```

A name that is not a function gets a suggestion, as a mistyped command does. `SELECT * FROM ursa_functions()` has the same information as columns, for programs.

`.describe NAME` takes the name as `.tables` and `.graphs` list it, in any case unless it is quoted: `.describe Mixed` describes a table created as `"Mixed"`. A qualified name (`s.t`) works too.

`.mode` with no argument shows the current mode, `.maxrows` how many rows a table shows, `.maxwidth` how wide a table may be, `.timer` whether the timer is on, and `.headers` whether CSV and TSV start with a header row.

`.schema` prints the statements that define the database's views, external tables and graphs, as the [database file](/docs/database/database-file/) stores them, in the order they are defined, each ending with `;`: paste them into another shell to define the same objects. Paths are as stored, relative to the database's directory. In a session without a database file, it prints the views' statements. `.schema PATTERN` keeps the objects whose name matches PATTERN, in any case: a `LIKE` pattern (`.schema %edges%`) or, as in DuckDB, a glob (`.schema karate*`).

`.output FILE` sends everything the shell prints to FILE, results and the text of dot-commands alike, until `.output` with no file sends it back to the screen. `.once FILE` does the same for the next command only (a statement, or a line of statements, or a dot-command), which is how to save the output of `SHOW GRAPHS` or `DESCRIBE GRAPH`, which `COPY … TO` cannot. The file is created, or emptied if it exists. Errors still go to the screen, and a table written to a file is as wide as it needs, without colour:

```text
memory U .mode markdown
memory U .once degrees.md
memory U SELECT * FROM degree(karate) ORDER BY degree DESC LIMIT 3;
```

A mistyped command, or the start of one (`.q`, `.tab`), gets a suggestion; it is not run:

```text
memory U .grpahs
Error: unknown command '.grpahs'; did you mean '.graphs'? Enter .help for the list
```

## Tables

A table is drawn as DuckDB draws one: each column's name with its type under it, numbers to the right and everything else to the left, and `NULL` for a null. The types are Arrow's, named as `DESCRIBE` and `arrow_typeof()` name them (`Int64`, `Utf8`, `Float64`, `List(Int64)`).

```text
memory U SELECT id, club FROM 'karate_nodes.csv' LIMIT 2;
┌───────┬────────┐
│  id   │  club  │
│ Int64 │  Utf8  │
├───────┼────────┤
│     0 │ Mr. Hi │
│     1 │ Mr. Hi │
└───────┴────────┘
```

In a terminal, types and nulls are dimmed; set `NO_COLOR` to turn colour off. Output that is not a terminal (a pipe, a file) has none.

The shell prints `table` by default. [Output formats](/docs/import-export/output/) covers the others: `csv`, `tsv`, `json`, `ndjson`, `markdown`, `line` and `arrow`. DuckDB's names `duckbox`, `box` and `jsonlines` work too.

### Long results

A table shows at most 40 rows, as DuckDB's does. A longer result shows its first 20 rows and its last 20, with rows of `·` between them, and a footer that counts the rows:

```text
memory U SELECT * FROM generate_series(1, 100000);
┌────────┐
│ value  │
│ Int64  │
├────────┤
│      1 │
│      2 │
…
│     20 │
│   ·    │
│   ·    │
│   ·    │
│  99981 │
…
│ 100000 │
└────────┘
  100,000 rows (40 shown)
```

`.maxrows N` (in the shell, a script or an init file) or `--max-rows N` (on the command line) changes how many: half from the start and half from the end. `.maxrows 0` and `--max-rows 0` show every row. The other modes, `csv`, `tsv`, `json`, `ndjson`, `markdown`, `line` and `arrow`, always write every row, so use one of them to save or read a whole result.

### Wide results

In a terminal a table is as wide as the terminal at most. A wider result first cuts its longest values short (`a long value…`), only as far as it needs to and no shorter than the column's name. If that is not enough, it leaves out columns from the middle, showing a `…` column in their place and saying so in the footer: `17 columns (10 shown)`. `.maxwidth N` (in the shell, a script or an init file) or `--max-width N` (on the command line) sets the width instead; `.maxwidth 0` goes back to the terminal's. Output that is not a terminal (a pipe, a file) is cut only when `.maxwidth N` or `--max-width N` sets a width.

## Init files

At startup the shell runs `~/.ursarc`, if it exists. It can hold SQL and dot-commands, one per line, as if typed at the prompt:

```text
.mode csv
.timer on
SET datafusion.execution.batch_size = 4096;
```

`-init FILE` runs `FILE` instead of `~/.ursarc`. `-no-init` runs neither. They are spelled with one dash, as in DuckDB; `--init` and `--no-init` work too.

`~/.ursarc` is read only by the interactive shell. `-c`, `--file` and SQL on stdin skip it, so a script gives the same output whoever runs it. `-init FILE` runs for those too, before their SQL. A `--format`, `--max-rows` or `--max-width` given on the command line wins over a `.mode`, `.maxrows` or `.maxwidth` in the init file.

`.read FILE` runs a file the same way, from the prompt.

## History

The shell keeps the lines you type in `~/.ursa_history`, so the up arrow reaches earlier sessions. To keep it elsewhere, set `URSA_HISTORY` to a file name. To keep none, set `URSA_HISTORY` to an empty string or start the shell with `--no-history`.
