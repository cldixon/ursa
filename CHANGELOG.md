# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The distribution is `ursa-graph`; the import name is `ursa`.

## [Unreleased]

### Changed

- `collect()` now hands the whole query to Rust as one plan. The frame is
  serialized to a plan IR in `ursa-plan` (`ursa_plan::plan::Plan`), and a single
  entry point, `execute_plan` (`ursa._ursa.run_plan` in Python), picks which
  executor runs it and rejects unsupported shapes, with the same error messages
  as before. The Python layer no longer decides which of the engine's seven entry
  points to call. Results are unchanged. Caches, subgraph masks, plain-source
  reads and the steps the engine's tail can't run in written order still run in
  Python, called back from the executor as it needs them. `run_node_query`,
  `run_join_query`, `run_hop_query`, `run_path_query`, `run_walk_query` and
  `graph_describe` are gone from `ursa._ursa`. `ursa-plan` gains plan
  conformance tests: JSON plans over karate and other small graphs, with
  expected output recorded from the Python package (#190, part of #189).

## [0.4.1] — 2026-10-19

### Fixed

- Unsorted output now has a fixed row order on the engine paths that had none:
  an attribute frame with graph algorithms (`ur.from_arrow(attrs, id=...)
  .with_columns(pr=ur.pagerank(edges))`) returns the attribute table's row
  order, and `join()` returns the left rows in order, each followed by its
  matches in right-frame order. `group_by().agg()` and `distinct()` return rows
  in order of first appearance, and `sample()` keeps its picks in input order,
  on every path including plain frames. `head(n)` takes the first `n` rows of
  that order. Before, the order changed with the number of CPUs (DataFusion's
  partition count) on large inputs (#168).

## [0.4.0] — 2026-10-18

### Added

- `sort()` takes several keys, Polars-style: `sort(["club", "pagerank"],
  descending=[False, True])` or `sort("a", "b")`. `descending` is one bool for
  every key or one per key; later keys break ties; nulls sort last for every
  key; dictionary-encoded keys sort by value. Works on plain, scanned and edge
  frames, grouped and joined results, graph-derived frames and traversal
  outputs (#163). The sort is stable on every path: rows that tie on every key
  keep their input order (#165).

- `direction=` on `density`, `avg_path_length`, `diameter`, `describe`,
  `pagerank`, `closeness` and `betweenness`. The default stays `"out"`, so
  existing results are unchanged. `direction="both"` reads the rows as a simple
  undirected graph: each pair of nodes joined in either direction is one edge,
  and self-loops are dropped. It matches NetworkX on `G.to_undirected()`: on
  Zachary's karate club, density 0.139, `avg_path_length` 2.41 and diameter 5,
  against 0.0695, 1.27 and 3 with the default (#124). `closeness` also takes
  `"in"`; `pagerank` and `betweenness` reject it. See *Semantics vs NetworkX*.
- `join()` and `group_by().agg()` now compose in one lazy pipeline, in either
  order, and run as written: `join(...).group_by(k).agg(...)` groups the joined
  rows, and `group_by(k).agg(...).join(...)` joins onto the grouped result.
  Previously this raised `NotImplementedError` and needed two `collect()` steps.
- `from_networkx(..., symmetrize=)` controls whether reverse rows are written.

### Fixed

- A `sort()` after a graph algorithm or traversal returned rows that tie on
  every key in an order that depended on the machine's core count, not in
  input order. Ties now keep their input order, as on a plain frame: node
  order (or the attribute table's row order) after a graph algorithm, and the
  traversal's own output order after `hop`/`shortest_path`/`random_walk`
  (#165).
- `select("c").distinct()` kept duplicate rows on plain frames, because
  `distinct` ran over every column before `select` narrowed them (#162).
  Other orderings that were silently reordered are fixed too: `head` before
  `sort`/`filter`/`distinct`/`sample`/`join` now takes the first rows as
  written, a rename that swaps two names no longer mislabels the columns, and
  two `sort`s on a graph-derived frame no longer drop the first.
- `rename(...).sort(new_name)` (and `filter`/`select`/`group_by` on the new
  name) failed with "unknown column", because rename was applied last (#164).

- A plain `collect()` on a `scan_edges` frame returned only the endpoint
  columns, so `filter`/`sort`/`select`/`group_by` on any other edge column
  failed with "unknown column", and `sink_parquet`/`sink_csv` wrote endpoints
  only. A scanned edge frame now has every column of the file, in file order
  and with the file's types, exactly like the same table loaded in memory;
  a `select()` is pushed down into the scan. Graph ops still read only the
  columns they need, so index-build memory is unchanged (#160). Scanned
  endpoints also keep their file type in relational output (an `int32` column
  stays `int32`) instead of being widened to `int64`.
- Weight expressions on a `scan_edges` frame can reference the `src`/`dst`
  columns; previously they failed with "No field named ..." (#154).
- `sort()` on a dictionary-encoded (categorical) column raised
  `ArrowNotImplementedError`, and so did `sum`/`mean`/`min`/`max` over one in
  `group_by().agg()`. Sorting now orders by the values (not the dictionary
  indices), and the column keeps its dictionary type in the output (#159).
- `scan_edges` and `scan_nodes` now return rows in file order on every run.
  A file large enough to be read as several parallel partitions previously
  came back in a run-dependent order, so dense ids, component labels,
  `edge_ids` and PageRank's last bits changed between runs over the same
  file. Output over a scan now matches `from_arrow` over the same table
  exactly (#148).
- `scan_edges` and `scan_nodes` over a Parquet file with string ids failed
  with "Utf8View is not a supported id type", even for a plain `string`
  column, because DataFusion reads Parquet strings as `Utf8View`. Scans now
  read string columns as the file declares them, and the id map reads
  `Utf8View` natively (no cast, so the 2 GiB `Utf8` cap is not involved).
- Every string layout now works as node ids, scanned or in memory: `string`,
  `large_string`, `string_view` (including tables that came from polars
  through the Arrow C stream) and dictionary-encoded (categorical) ids, which
  previously raised `TypeError` or "Dictionary(Int32, Utf8) is not a supported
  id type".
- `filter()`/`sort()` on a `scan_nodes` frame with a string attribute column
  failed with `ArrowNotImplementedError` (no `string_view` kernel); string
  attributes now arrive as `string`. `neighbors().agg(n_unique=...)` over a
  dictionary-encoded string attribute is treated as a string, not cast to a
  number.
- A kernel that asked for the lazily built transpose or undirected view from
  inside a parallel loop could deadlock, e.g. `neighbors().agg()` or `hop` with
  `direction="in"`/`"both"` on graphs above 65,536 edges.

### Changed

- **Behaviour change:** relational steps (`filter`, `select`, `rename`,
  `sort`, `head`, `distinct`, `sample`, `group_by().agg()`, `join`) now take
  effect in the order they are written, as in Polars, on every collect path.
  Previously each path applied them in a fixed internal order, which gave
  wrong results for some orderings (see Fixed) and rejected others. Orderings
  that used to raise now run as written: `sort`/`head`/`rename` before
  `group_by`, a `filter` after `group_by` (SQL `HAVING`), `select` after
  `group_by`, chained `rename`s, and `rename` then `join` on the new key. A
  few pipelines that used to succeed incorrectly now raise: a `filter` on a
  column's old name after `rename`, a `filter` on a column that an earlier
  `select` dropped, and a non-filter step on the edges before `nodes()`. The
  common filter/sort/head/select tail on graph-derived frames still runs in
  DataFusion.

- **Behaviour change:** `from_networkx` now writes each edge of an undirected
  `nx.Graph`/`nx.MultiGraph` in both directions (a self-loop stays one row), so
  the default `direction="out"` gives NetworkX's undirected values for
  `pagerank`, `closeness`, `density`, `diameter` and the rest. Previously each
  edge was written once, which silently produced directed results. Pass
  `symmetrize=False` for the old one-row-per-edge frame. `nx.DiGraph` input is
  unchanged. On a symmetrized frame `betweenness` counts each unordered pair
  twice with the default; pass `direction="both"` to match NetworkX (#124).
- `.to_polars()` no longer appears in first-contact examples (the module and
  `ursa.datasets` docstrings, the Quickstart, `examples/quickstart.py`), which
  now print `collect()` or use `.to_arrow()`/`.to_dicts()`, so they run on a
  bare install. Polars remains an optional extra; its one example lives in the
  Output guide.
- The package summary (`pip show ursa-graph`) and the README install line now
  name the import name, `ursa`.

- Building the graph index over an unweighted `scan_edges` source no longer
  holds the scanned Arrow endpoints for the whole build. Each batch is interned
  and freed as it is decoded, inside one native call, so the 16 B/edge input
  never accumulates. On a 30M-edge Parquet file the build's peak memory fell
  from about 45 to 24 B/edge, and the footprint after the build from about 33
  to 17 B/edge, with build time unchanged (#149). Weighted scans keep the
  previous path, since their weight columns must outlive the build.
- Faster and leaner throughout; every kernel's output is unchanged unless
  noted. Timings on a 4-core machine:
  - Index build: known ids are interned in parallel, and scans read 128K-row
    batches. A 30M-edge Parquet scan and build went from ~2.9s to ~1.65s;
    with 2M string ids, from ~8.5s to ~3.1s.
  - In-memory frames (`from_arrow`, `from_polars`, `EdgeFrame(...)`, ...)
    no longer keep a concatenated copy of the `src`/`dst` columns next to the
    table; they share its chunks. On 30M int edges the build went from 2.7s
    to 1.6s (27 to 22 B/edge over the table); on 20M string edges, from 6.9s
    and 71 B/edge to 1.9s and 24 B/edge.
  - String node ids are stored once, in the id column's own buffers, instead
    of about 140 B of per-id overhead. On 20M edges and 2M ids the build peak
    fell from 36 to 27 B/edge.
  - The transpose that in-direction kernels use is built straight from the
    out-CSR, without per-row endpoint arrays, so it is no longer the peak:
    35 to 30 B/edge on a 30M-edge graph.
  - `triangle_count` uses the degree-ordered forward algorithm (6–7× faster
    on skewed graphs), and `clustering_coefficient` benefits too.
  - `label_propagation` (6×) and `louvain` (3–4×) count votes and community
    weights in dense arrays instead of hash maps. Louvain's aggregation now
    sums in a fixed order rather than a randomly seeded hash map's.
  - `pagerank` divides by out-degree once per node, not per edge (1.8×).
  - Weight expressions are compiled once and evaluated per batch, and
    columns that share an expression share one evaluation.
  - Smaller items: masks built from packed bits, a bounded CSR-build
    histogram on many-core machines, and a leaner strongly-connected-components
    kernel.
- An edge mask whose length does not match the graph's edge count is rejected
  instead of silently dropping the edges past its end.

### Security

- Bumped `rustls` (pulled in by the object-store TLS stack used for
  `s3://`/`gs://`/`az://`/`https://` scans) from 0.23.45 to 0.23.46 for
  RUSTSEC-2026-0346, -0347, -0348 and -0349.

## [0.3.0] — 2026-10-08

### Added

- `ursa.datasets`: bundled graphs that load offline from the wheel (`karate`
  with faction labels, `lesmis` weighted, `florentine`, `kite`), plus
  `facebook` (SNAP ego-Facebook), downloaded on first use and cached under
  `$URSA_DATA_HOME`. `list_datasets()` returns the registry.
- Interop constructors `from_edgelist`, `from_networkx`, `nodes_from_networkx`,
  `from_numpy` and `from_scipy_sparse`. networkx, numpy and scipy remain
  optional and are imported only when the matching constructor is called.
- `group_by(*keys).agg(...)`, with `mean`, `sum`, `min`, `max`, `count` and
  `n_unique` aggregations, optionally aliased or named through `agg(name=...)`.
- `join(other, on=, how=)`: an equi-join of two frames on shared key columns,
  `inner` or `left`. Distinct from the automatic attribute attach.
- `rename(mapping)` and `sample(n, seed=)` on the shared relational tail.
- Filter predicates now lower as a full algebra: string and numeric
  comparisons, boolean `&` / `|` / `~`, arithmetic, and column-to-column
  comparisons. Previously only a single `col <op> number` comparison executed.
- Graph operations over a filtered edge frame (subgraph views), and over a
  traversal result from `hop` or `shortest_path`.
- `dtype="f32"` on the float-valued kernels (`pagerank`, `closeness`,
  `betweenness`, `clustering_coefficient`) and on `neighbors().agg()`, which
  emits the result column as 32-bit float. The kernel still accumulates in f64
  and the emitted value is that result cast down, so it agrees within f32
  precision. The integer-valued kernels take no `dtype`. The default output is
  unchanged.
- `http(s)://` reads for a single hosted Parquet or CSV file.
- `on_null="drop"` on edge inputs (`EdgeFrame`, `from_arrow`, `from_edgelist`,
  `scan_edges`, `read_edges`), which filters rows with a null `src` or `dst`
  and reports the dropped count as a warning. The default remains `"error"`.
- Documentation site at <https://cldixon.github.io/ursa/>: a quickstart, one
  guide per task, a reference, a limits page, and a semantics reference that
  pins each kernel to the NetworkX call it is checked against. Every example
  on the site runs in the test suite.

### Changed

- A query naming several graph algorithms now shares work across its columns
  instead of recomputing per column. Two columns naming the same kernel with the
  same parameters run it once — including the same kernel at two output dtypes,
  since `dtype=` narrows on emit and so is not part of a computation's identity.
  Separately, `triangle_count` and `clustering_coefficient` in one
  `with_columns` now share the sorted-adjacency intersection pass they both rest
  on, where previously each ran its own; over a subgraph view they also share the
  masked undirected adjacency, which is rebuilt per use rather than cached. Values
  are unchanged and bit-identical either way.
- `rayon` is now an on-by-default feature of the `ursa-core` crate. With it
  disabled the crate compiles for `wasm32-unknown-unknown`, single-threaded, and
  `rand`'s default features — and so `getrandom` — are dropped, since the
  stochastic kernels use only seeded ChaCha streams. Serial and parallel outputs
  are bit-identical. This affects Rust consumers of `ursa-core` only: the Python
  wheels build with default features and are unchanged.
- `repr()` of a collected frame renders the head as a table with the shape,
  column names and dtypes, capped at 10 rows. It previously showed only a
  shape and a column list, which left results opaque in a REPL. The preview
  reads only the rows it displays, so its cost does not scale with row count.
- `to_polars()` raises a `ModuleNotFoundError` naming the `ursa-graph[polars]`
  extra when polars is absent, in place of a bare import error.
- The README documents installation, and the PyPI project metadata links the
  documentation site.
- `version`, `ModuleType` and `PackageNotFoundError` are no longer exposed in
  the `ursa` namespace. `__version__` and `__core_version__` are unchanged.

### Fixed

- `edges.filter(...).collect()` and `edges.distinct().collect()` return rows;
  both previously raised `NotImplementedError`.

## [0.2.0] — 2026-08-01

Performance improvements and syntax standardization.

## [0.1.1] — 2026-07-23

Fixed a missing Arrow dependency.

## [0.1.0] — 2026-07-21

Initial release.

[Unreleased]: https://github.com/cldixon/ursa/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/cldixon/ursa/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cldixon/ursa/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/cldixon/ursa/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cldixon/ursa/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cldixon/ursa/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cldixon/ursa/releases/tag/v0.1.0
