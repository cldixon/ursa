# The shell

`ursa` with no SQL to run starts an interactive shell. Give it a database file to keep what you define, or nothing to work in memory:

```bash
ursa club.ursa
ursa
```

```text
ursa 0.6.0, database club.ursa. Enter .help for commands, .quit to exit.
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

`-c` and `-f` (`--file`) may be repeated; they run in the order given. If any statement fails, `ursa` prints the error, stops, and exits with status 1.

Add `--readonly` to open a database without changing it. See [the database file](database.md#read-only-and-sharing).

## Dot-commands

A line that starts with `.` is a shell command, not SQL. The commands follow DuckDB's shell where it has the same one.

```text
ursa> .help
Enter SQL statements ending with ';'. A statement may span lines.
Ctrl-C cancels the current input or query; Ctrl-D exits.

.describe NAME        Describe a graph, table or view
.exit                 Exit the shell (also Ctrl-D)
.functions            List the graph functions and their parameters
.graphs               List the graphs (SHOW GRAPHS)
.help                 Show this message
.mode MODE            Set the output mode: table, csv, tsv, json or ndjson
.open FILE.ursa       Open a database file, before anything is defined (as ATTACH)
.quit                 Exit the shell (also Ctrl-D)
.read FILE            Run the SQL and dot-commands in FILE
.tables               List the tables and views
.timer on|off         Show how long each statement takes
```

| Command           | Same as                                    |
| ----------------- | ------------------------------------------ |
| `.graphs`         | `SHOW GRAPHS;`                             |
| `.describe karate` | `DESCRIBE GRAPH karate;` for a graph, `DESCRIBE t;` for a table or view |
| `.functions`      | `SELECT * FROM ursa_functions();`          |
| `.open club.ursa` | `ATTACH 'club.ursa';`                      |
| `.tables`         | a query on `information_schema.tables`, without the graphs' own tables |

`.mode` with no argument shows the current mode. `.timer` with no argument shows whether the timer is on. A mistyped command gets a suggestion:

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

## Init files

At startup the shell runs `~/.ursarc`, if it exists. It can hold SQL and dot-commands, one per line, as if typed at the prompt:

```text
.mode csv
.timer on
SET datafusion.execution.batch_size = 4096;
```

`-init FILE` runs `FILE` instead of `~/.ursarc`. `-no-init` runs neither. They are spelled with one dash, as in DuckDB; `--init` and `--no-init` work too.

`~/.ursarc` is read only by the interactive shell. `-c`, `--file` and SQL on stdin skip it, so a script gives the same output whoever runs it. `-init FILE` runs for those too, before their SQL. A `--format` given on the command line wins over a `.mode` in the init file.

`.read FILE` runs a file the same way, from the prompt.

## History

The shell keeps the lines you type in `~/.ursa_history`, so the up arrow reaches earlier sessions. To keep it elsewhere, set `URSA_HISTORY` to a file name. To keep none, set `URSA_HISTORY` to an empty string or start the shell with `--no-history`.
