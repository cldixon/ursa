# The shell

`ursa` with no SQL to run starts an interactive shell. Give it a database file to keep what you define, or nothing to work in memory:

```bash
ursa club.ursa
ursa
```

```text
ursa 0.8.0, database club.ursa. Enter .help for commands, .quit to exit.
ursa>
```

Type SQL at the prompt. A statement ends with `;` and may span lines; the prompt changes while a statement is unfinished. Results print as tables. An error prints a message and the session goes on.

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

Ctrl-C (SIGINT) cancels the run: `ursa` stops the query that is running, prints `Error: interrupted` to stderr, writes nothing more to stdout, and exits with status 130. Results that finished before it stay printed. The database is left as the last finished statement left it, and another process can open it at once. In the shell, Ctrl-C cancels the running statement and returns to the prompt.

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

Statements that return rows always print a result, with its header even when there are no rows. Statements that return no columns, which are DDL (`CREATE`, `DROP`) and `SET`, print nothing, as in DuckDB. In `ndjson`, which has no header, a result with no rows prints nothing.

Add `--readonly` to open a database without changing it. See [the database file](database.md#read-only-and-sharing).

Notices, such as a slow graph index build or an index the database could not keep, go to stderr as lines starting `ursa: `, so stdout holds only results. `--quiet` (`-q`) turns them off. See [notices](database.md#notices).

## Dot-commands

A line that starts with `.` is a shell command, not SQL. The commands follow DuckDB's shell where it has the same one.

```text
ursa> .help
Enter SQL statements ending with ';'. A statement may span lines.
Ctrl-C cancels the current input or query; Ctrl-D exits.

.describe NAME        Describe a graph, table or view
.exit                 Exit the shell (also Ctrl-D)
.functions [NAME]     List the graph functions, or show one's parameters
.graphs               List the graphs (SHOW GRAPHS)
.help                 Show this message
.maxrows N            Show at most N rows of a table (default 40; 0 shows all)
.mode MODE            Set the output mode: table, csv, tsv, json or ndjson
.open FILE.ursa       Open a database file, before anything is defined (as ATTACH)
.quit                 Exit the shell (also Ctrl-D)
.read FILE            Run the SQL and dot-commands in FILE
.tables               List the tables and views
.timer on|off         Show how long each statement takes (real, user and sys)

Any file is a table: SELECT * FROM 'edges.csv' LIMIT 3;
An edge table is a graph:
  CREATE PROPERTY GRAPH g EDGE TABLES (ties SOURCE KEY (src) DESTINATION KEY (dst));
SELECT * FROM ursa_functions(); lists the graph functions (as .functions does).
Docs: https://cldixon.github.io/ursa/sql/
```

| Command           | Same as                                    |
| ----------------- | ------------------------------------------ |
| `.graphs`         | `SHOW GRAPHS;`                             |
| `.describe karate` | `DESCRIBE GRAPH karate;` for a graph, `DESCRIBE t;` for a table or view |
| `.functions`      | `SELECT name, family, summary FROM ursa_functions();`, with the first sentence of each summary |
| `.open club.ursa` | `ATTACH 'club.ursa';`                      |
| `.tables`         | a query on `information_schema.tables`, without the graphs' own tables |

`.functions NAME` shows one function: its summary, then its parameters one a line (name, type, default, what it does, and the values it takes), then its output columns:

```text
ursa> .functions degree
degree (algorithm)
Per-node degree.
NetworkX: G.out_degree() (in: G.in_degree(); both: U.degree() on a simple graph)

Parameters, after the graph, as name => value:
  direction  string  default 'out'  Follow out-edges, in-edges, or both (the undirected view). One of: out, in, both.

Output columns:
  degree  UInt32
```

A name that is not a function gets a suggestion, as a mistyped command does. `SELECT * FROM ursa_functions()` has the same information as columns, for programs.

`.mode` with no argument shows the current mode, `.maxrows` how many rows a table shows, and `.timer` whether the timer is on. A mistyped command gets a suggestion:

```text
ursa> .grpahs
Error: unknown command '.grpahs'; did you mean '.graphs'? Enter .help for the list
```

## Output modes

The shell prints `table` by default. `.mode csv`, `.mode tsv`, `.mode json` and `.mode ndjson` switch it. On the command line, `--format` sets the mode for the shell and for `-c` alike:

```bash
ursa club.ursa --format json -c "SELECT * FROM degree(karate) ORDER BY id LIMIT 2"
```

```text
[{"id":0,"degree":16},{"id":1,"degree":8}]
```

`--format arrow` writes an Arrow IPC stream, for another program to read. It works only with `-c` or `-f`, and only for one query that returns rows.

### Long results

A table shows at most 40 rows, as DuckDB's does. A longer result shows its first 20 rows and its last 20, with rows of `·` between them, and a footer that counts the rows:

```text
ursa> SELECT * FROM generate_series(1, 100000);
+--------+
| value  |
+--------+
| 1      |
| 2      |
…
| 20     |
| ·      |
| ·      |
| ·      |
| 99981  |
…
| 100000 |
+--------+
100,000 rows (40 shown)
```

`.maxrows N` (in the shell, a script or an init file) or `--max-rows N` (on the command line) changes how many: half from the start and half from the end. `.maxrows 0` and `--max-rows 0` show every row. The other modes, `csv`, `tsv`, `json`, `ndjson` and `arrow`, always write every row, so use one of them to save or read a whole result.

## Init files

At startup the shell runs `~/.ursarc`, if it exists. It can hold SQL and dot-commands, one per line, as if typed at the prompt:

```text
.mode csv
.timer on
SET datafusion.execution.batch_size = 4096;
```

`-init FILE` runs `FILE` instead of `~/.ursarc`. `-no-init` runs neither. They are spelled with one dash, as in DuckDB; `--init` and `--no-init` work too.

`~/.ursarc` is read only by the interactive shell. `-c`, `--file` and SQL on stdin skip it, so a script gives the same output whoever runs it. `-init FILE` runs for those too, before their SQL. A `--format` or `--max-rows` given on the command line wins over a `.mode` or `.maxrows` in the init file.

`.read FILE` runs a file the same way, from the prompt.

## For coding agents

`ursa --skill` prints a guide to writing Ursa SQL, in Markdown: an [agent skill](https://github.com/cldixon/ursa/tree/main/skills/ursa-sql) built into the binary, so it always matches the version you run. An agent can read it before writing queries, or you can save it where your agent loads skills, for example for Claude Code:

```bash
mkdir -p ~/.claude/skills/ursa-sql
ursa --skill > ~/.claude/skills/ursa-sql/SKILL.md
```

## History

The shell keeps the lines you type in `~/.ursa_history`, so the up arrow reaches earlier sessions. To keep it elsewhere, set `URSA_HISTORY` to a file name. To keep none, set `URSA_HISTORY` to an empty string or start the shell with `--no-history`.
