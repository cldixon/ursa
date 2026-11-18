---
title: How-to Guides
description: Short answers to common tasks with Ursa SQL, each pointing to the section that does it.
---

Each guide is a section of the docs that does one task, with SQL that runs. They use the karate club and Les Misérables datasets; the [Quickstart](/docs/quickstart/) shows how to download them.

## Install and run

- [Install `ursa` with the install script](/docs/installation/#install-script), [with Cargo](/docs/installation/#cargo), or [from a prebuilt binary](/docs/installation/#prebuilt-binaries).
- [Try a nightly build](/docs/installation/#nightly-builds).
- [Run SQL from a script, a file or stdin](/docs/cli/command-line/#running-sql).
- [Keep settings and dot-commands in an init file](/docs/cli/shell/#init-files).
- [Teach a coding agent to write Ursa SQL](/docs/cli/command-line/#for-coding-agents).

## Import and export data

- [Query a CSV, Parquet or JSON file](/docs/quickstart/#query-a-file).
- [Read a file with options, whatever its extension](/docs/import-export/files/#read_parquet-read_csv-and-read_json).
- [Write a query's rows to a file with `COPY`](/docs/import-export/files/#reading-and-writing-files).
- [Convert a large CSV file to Parquet once](/docs/database/database-file/#what-it-keeps).
- [Query files in S3, GCS or Azure](/docs/import-export/object-storage/), and [set up credentials](/docs/import-export/object-storage/#credentials).
- [Read results as JSON in a program](/docs/import-export/output/#reading-results-in-a-program).

## Define graphs

- [Define a graph from an edge table](/docs/quickstart/#define-a-graph).
- [Give a graph node attributes with a vertex table](/docs/graphs/property-graphs/#create-property-graph).
- [Define a graph of several tables](/docs/graphs/property-graphs/#several-tables).
- [Work on a subgraph](/docs/graphs/algorithms/#subgraphs).
- [List and describe your graphs](/docs/graphs/property-graphs/#show-graphs).

## Analyze graphs

- [Run PageRank and rank the nodes](/docs/quickstart/#run-an-algorithm).
- [Run several algorithms in one query and join attributes](/docs/quickstart/#combine-algorithms-and-attributes).
- [Treat a directed graph as undirected](/docs/graphs/algorithms/#direction).
- [Weight the edges](/docs/graphs/algorithms/#weights).
- [Aggregate a value over each node's neighbours](/docs/graphs/algorithms/#aggregate-over-neighbours).
- [Find the nodes within n steps](/docs/graphs/traversals/#hop).
- [Find the shortest path between two nodes](/docs/graphs/traversals/#shortest_path), or [every shortest path](/docs/graphs/pattern-matching/#which-paths).
- [Match a pattern of nodes and edges](/docs/graphs/pattern-matching/#a-first-match).
- [Make a pattern match fast](/docs/graphs/pattern-matching/#what-is-fast).

## Databases

- [Keep views, graphs and indexes in a database file](/docs/quickstart/#open-a-database).
- [Query a database while another process writes it](/docs/database/database-file/#read-only-and-sharing).
- [Limit the space stored indexes take](/docs/database/database-file/#the-storage-limit).
- [Check whether a query built or loaded its index](/docs/database/database-file/#stored-graph-indexes).
- [Move a database with its data](/docs/database/database-file/#on-disk).

## Coming from DuckDB

- [What works the same](/docs/duckdb/#what-works-the-same), and [what is different](/docs/duckdb/#what-is-different).
