# SQL/PGQ and DuckPGQ conformance suite

The cases that check Ursa against SQL/PGQ and DuckPGQ (#299). `cargo test -p ursa-sql --test conformance` runs them, as part of the `ursa-sql` tests.

- `sqlpgq/`: cases written from the standard and from the SQL surface review (#408), on DuckPGQ's canonical graph.
- `known_differences.toml`: the reviewed list of cases where Ursa differs, each with its kind and reason.

## Format

sqllogictest, as DuckDB, DuckPGQ and DataFusion write their tests. Each file runs in a fresh session. A record is one of:

```text
# id: match/1  a comment after the id
query II rowsort
SELECT … ;
----
0 1
0 2

statement ok
CREATE … ;

statement error <regex>
SELECT … ;
```

Values in a row are separated by one space, `NULL` is `NULL`, and `rowsort` compares the rows sorted. The comment `# id: <dir>/<name>` names the record that follows; ids are unique, and the known-differences list refers to them.

**Expected results are the reference's**, not Ursa's: the standard's, or DuckPGQ's where the standard leaves it open. When Ursa differs, the case goes on the list; the file is not edited to match Ursa.

## The rules

1. A case not on the list must give the reference's result.
2. A case on the list must still differ, in the recorded way: Ursa's error matches its `error` regex. A case that starts to pass fails the suite with "remove this entry", so the list stays true and every `unsupported` entry is a test of its error message.
3. A `bug` entry needs an open issue.
4. A debug dump (`AggregateFunction {`, `Physical plan does not support`) is never an accepted difference.

The test prints a table of cases, passes and known differences per directory.

## Adding a case

Add a record with a new id to a file (or a new file) under a directory named for its source. Run the suite. If Ursa differs, add an entry to `known_differences.toml`:

```toml
[[difference]]
id     = "match/11"
kind   = "unsupported"      # unsupported | deliberate | numeric-reference | bug
issue  = 412
reason = "A variable over several element tables."
error  = "cannot be told from the pattern"
```
