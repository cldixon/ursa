# The shell

`ursa` with no SQL to run starts an interactive shell. Give it a database file to keep what you define, or nothing to work in memory:

```bash
ursa club.ursa
ursa
```

```text
ursa 0.13.0, database club.ursa. Enter .help for commands, .quit to exit.
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

## Running SQL without the shell

The shell starts only when there is no SQL to run and the input is a terminal. These run SQL and exit:

```bash
ursa club.ursa -c "SELECT * FROM degree(karate)"   # one or more statements
ursa club.ursa -f query.sql                        # a file of SQL (or --file)
echo "SHOW GRAPHS;" | ursa club.ursa               # SQL on stdin
```

`-c` and `-f` (`--file`) may be repeated; they run in the order given. If any statement fails, `ursa` prints the error, stops, and exits with status 1. A syntax error anywhere in a `-c`, a file or stdin runs none of it.

Ctrl-C (SIGINT) cancels the run: `ursa` stops the query that is running, prints `Error: interrupted` to stderr, writes nothing more to stdout, and exits with status 130. Results that finished before it stay printed. The database is left as the last finished statement left it, and another process can open it at once. In the shell, Ctrl-C cancels the running statement, prints `Interrupted` to stderr and draws the prompt again at once.

A script may hold dot-commands too, one a line, as at the prompt and as in DuckDB:

```text
.timer on
.mode csv
SELECT count(*) AS edges FROM 'karate_edges.csv';
```

```bash
ursa -f q.sql
```

```text
edges
78
Run Time (s): real 0.004 user 0.006113 sys 0.001024
```

The timer reports, as DuckDB's does, the wall-clock time (`real`) and the CPU time the process spent in its own code (`user`) and in the operating system (`sys`). Those two add up the time of every thread, so a query that runs in parallel can use more CPU time than real time.

`.read FILE` in a script runs another script. In a script, `.quit` and `.exit` end the run there, with status 0. An unknown dot-command is an error, as a failing statement is. With `--format arrow`, the output is one Arrow stream: text a dot-command prints, such as the timer's, goes to stderr instead.

Statements that return rows always print a result, with its header even when there are no rows. Statements that return no columns, which are DDL (`CREATE`, `DROP`) and `SET`, print nothing, as in DuckDB. In `ndjson`, which has no header, a result with no rows prints nothing. A statement that fails prints nothing to stdout, not even its header, so a script never reads a header with no rows for a query that failed; the error goes to stderr. (Rows are written as they arrive, so a query that fails after its first rows have been written has written them.)

Add `--readonly` to open a database without changing it. See [the database file](database.md#read-only-and-sharing).

Notices, such as a slow graph index build or an index the database could not keep, go to stderr as lines starting `ursa: `, so stdout holds only results. `--quiet` (`-q`) turns them off. See [notices](database.md#notices).

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

`.schema` prints the statements that define the database's views, external tables and graphs, as the [database file](database.md) stores them, in the order they are defined, each ending with `;`: paste them into another shell to define the same objects. Paths are as stored, relative to the database's directory. In a session without a database file, it prints the views' statements. `.schema PATTERN` keeps the objects whose name matches PATTERN, in any case: a `LIKE` pattern (`.schema %edges%`) or, as in DuckDB, a glob (`.schema karate*`).

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
networkx = nx.pagerank(G, alpha=damping) (in: nx.pagerank(G.reverse()); both: nx.pagerank(U))
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

`csv`, `tsv`, `json`, `ndjson`, `markdown`, `line` and `arrow` write every row and never cut a value. A `table` shows at most 40 rows (see [Long results](#long-results)).

### Tables

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

## For coding agents

`ursa --skill` prints a guide to writing Ursa SQL, in Markdown: an [agent skill](https://github.com/cldixon/ursa/tree/main/skills/ursa-sql) built into the binary, so it always matches the version you run. An agent can read it before writing queries, or you can save it where your agent loads skills, for example for Claude Code:

```bash
mkdir -p ~/.claude/skills/ursa-sql
ursa --skill > ~/.claude/skills/ursa-sql/SKILL.md
```

An agent should read results as `--format json` or `--format ndjson` (see [Reading results in a program](#reading-results-in-a-program)): a table shows at most 40 rows, while CSV and JSON write every row.

## In a browser tab: `ursa serve`

`ursa serve` runs the same shell in a browser tab on your machine, over a local WebSocket to this native engine, with the graph view beside it:

```bash
ursa serve                 # in memory, in the current directory
ursa serve data/           # relative paths in queries are data/'s
ursa serve club.ursa       # the database, as `ursa club.ursa` opens it
```

It prints a link with a one-time token and opens it in your default browser; `--no-open` only prints it. It listens on `127.0.0.1` and `::1` only, on port 4280 or the next free one (`--port N`). Ctrl+C stops it.

A directory, or a database file's own directory, is where relative paths resolve, and where the server writes: `COPY … TO`, `.output` and `.once` take a relative path under it. The shell's tab keeps what `COPY … TO 'name'` writes in the session, as the [Ursa Shell](https://ursa-shell.cl-dixon.workers.dev) in the browser does (`.files` lists them).

`--read-only` writes no files, for pairing with an agent: `COPY … TO`, `INSERT INTO` a table of files, `.output`, `.once`, `ATTACH` of a database that does not exist, and anything that would change the database (opened read-only) are refused. Views and tables in memory still work.

The release binaries (the installers' and GitHub's) have the page built in. A build with `cargo install ursa-cli --features serve` has not: pass `--assets DIR`, a `web/dist` built from Ursa's repository.

## History

The shell keeps the lines you type in `~/.ursa_history`, so the up arrow reaches earlier sessions. To keep it elsewhere, set `URSA_HISTORY` to a file name. To keep none, set `URSA_HISTORY` to an empty string or start the shell with `--no-history`.
