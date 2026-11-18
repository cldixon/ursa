---
title: "Command line"
description: "Run SQL with ursa -c, -f and stdin, in scripts, and from coding agents."
---

`ursa` with SQL to run runs it and exits, as `duckdb -c` does. In a terminal with nothing to run it starts [the shell](/docs/cli/shell/).

## Running SQL

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

Statements that return rows always print a result, with its header even when there are no rows. Statements that return no columns, which are DDL (`CREATE`, `DROP`) and `SET`, print nothing, as in DuckDB. In `ndjson`, which has no header, a result with no rows prints nothing.

Add `--readonly` to open a database without changing it. See [the database file](/docs/database/database-file/#read-only-and-sharing).

Notices, such as a slow graph index build or an index the database could not keep, go to stderr as lines starting `ursa: `, so stdout holds only results. `--quiet` (`-q`) turns them off. See [notices](/docs/database/database-file/#notices).

## For coding agents

`ursa --skill` prints a guide to writing Ursa SQL, in Markdown: an [agent skill](https://github.com/cldixon/ursa/tree/main/skills/ursa-sql) built into the binary, so it always matches the version you run. An agent can read it before writing queries, or you can save it where your agent loads skills, for example for Claude Code:

```bash
mkdir -p ~/.claude/skills/ursa-sql
ursa --skill > ~/.claude/skills/ursa-sql/SKILL.md
```

An agent should read results as `--format json` or `--format ndjson` (see [Reading results in a program](/docs/import-export/output/#reading-results-in-a-program)): a table shows at most 40 rows, while CSV and JSON write every row.
