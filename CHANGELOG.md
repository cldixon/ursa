# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The distribution is `ursa-graph`; the import name is `ursa`.

## [Unreleased]

### Added

- Criterion micro-benchmarks for the `ursa-core` kernels (`cargo bench -p
  ursa-core`, `ursa-core/benches/kernels.rs`): topology build and views, id
  interning, and every kernel family on seeded Erdős–Rényi and R-MAT graphs up
  to 1M edges. A `Bench` workflow runs them weekly and on demand;
  `benchmarks/README.md` describes comparing two commits (#187).

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
- One expression evaluator: every `filter` now runs in Rust through the same
  DataFusion lowering, wherever it sits. Before, `pyarrow.compute` evaluated
  filters on plain frames, subgraph-view masks and steps after the engine's
  tail. Subgraph masks are built in Rust, in dense id space; a traversal
  result's mask comes straight from the reached node set. Plain sources are
  DataFusion scans: `scan_edges(...).filter(...)` pushes its leading filters
  into the read, so Parquet skips row groups whose statistics rule them out.
  Masks are cached on the graph index, keyed by the view, so two frames with
  the same filter share one. A `scan_nodes` frame keeps one table, the union
  of the columns read so far, instead of one table per projection (#178).
  Results are unchanged, except for NaN (next entry) (#192, part of #189). On 10M
  edges (4 cores): the first masked `degree` over a scan, 0.24s to 0.11s; the
  mask of a 2-hop traversal result from 2,000 seeds, 0.92s to 0.24s; a
  1%-selective filter on a 10M-row Parquet file sorted by the filtered column,
  0.12s to 0.010s.
- **Behaviour change:** NaN compares in total order in every filter: NaN equals
  NaN and is greater than every number, as in `sort()`. Filters inside an
  engine query already did; filters on plain frames, subgraph masks and steps
  after the engine's tail compared IEEE-style (any comparison with NaN false,
  `!=` true). With `f = [0.5, NaN, null, 2.0, -1.0]`, a plain frame keeps:

  | filter | before | after |
  |---|---|---|
  | `f > 1` | `2.0` | `NaN, 2.0` |
  | `~(f > 1)` | `0.5, NaN, -1.0` | `0.5, -1.0` |
  | `f == nan` | none | `NaN` |
  | `f != nan` | `0.5, NaN, 2.0, -1.0` | `0.5, 2.0, -1.0` |
  | `f != f` | `NaN` | none |
  | `f / 0 > 1` | `0.5, 2.0` | `0.5, NaN, 2.0` |

  Nulls are unchanged: a comparison with null is null, and the row is dropped
  (#199).
- Filters inside an engine query (after graph columns, a join or a traversal)
  accept every literal a plain-frame filter does: datetimes, dates, times,
  timedeltas, `numpy.datetime64`/`timedelta64` (a unit of a day or finer),
  `Decimal` (up to 76 digits), `bytes`, NaN, infinity and `None`. They used to
  raise `TypeError: Object of type ... is not JSON serializable` (or
  `ComputeError` for NaN and infinity, "unsupported literal type" for `None`).
  A day-unit `numpy.datetime64` now works on plain frames too (it raised a
  `TypeError`); a month or year unit raises a `TypeError` that says so (#192).
- Every filter compares the way plain frames did: operand types pyarrow had no
  comparison kernel for stay errors, timestamps, durations and times compare
  at the finer unit, an integer past 2**53 against a float and a `uint64` past
  2**63 against a signed integer stay "not in range" errors. Engine filters
  used DataFusion's coercion, which parsed `ur.col("n") == "1"` as a number and
  compared timestamps at the coarser unit (#192).
- Such errors are `ursa.ComputeError` on every path. Plain frames and subgraph
  masks used to raise pyarrow's exceptions: `ArrowNotImplementedError` (a
  `NotImplementedError` subclass) for a type mismatch such as
  `ur.col("n") == "x"`, and `ArrowInvalid` (a `ValueError` subclass) for an
  integer divide by zero or a value "not in range". The engine already raised
  `ComputeError`. A timezone-aware against a naive timestamp of the same unit
  stays a `ValueError` (#192).
- A `string_view` column compares with a string literal; pyarrow had no kernel
  for it (#192).
- `ursa._ursa` no longer has `hop_reached_nodes_query`,
  `shortest_path_nodes_query`, `scan_edges_arrow`, `scan_edges_table_arrow` or
  `scan_nodes_arrow`; the plan reads and masks itself (#192).
- **Behaviour change:** `louvain` partitions differ from 0.4.1 for the same
  seed. Local moving now revisits only the neighbours of nodes that moved, and a
  level stops when a sweep's worth of work gains less than 1e-7 modularity (with
  a hard cap of 64 sweeps' worth), instead of sweeping until no node moves.
  Modularity is equal or higher on most graphs; results stay deterministic for a
  given seed and identical on any thread count. Louvain is now near-linear in
  graph size: 1M edges takes 2.2s, down from about 6 minutes (#125, #185).
- Faster and leaner; outputs are byte-identical unless noted. Timings on a
  4-core machine:
  - Repeated weighted collects over a `scan_edges` frame no longer rescan the
    file and rebuild the index. The evaluated `weight=` vector is cached on the
    index (up to two expressions), so a repeat skips the read entirely: weighted
    PageRank over a 10M-edge scan, 2.2s to 1.4s, and peak memory 1.0 to 0.68 GB
    (#179).
  - `direction="both"` builds its symmetric graph once per index, straight from
    the cached undirected adjacency, and it is its own transpose; the weighted
    view is built flat and shared within a query. PageRank with
    `direction="both"` on 10M edges: 0.91s to 0.34s; weighted, 4.3s to 1.4s and
    1.9 to 1.2 GB peak (#180).
  - A filtered or traversal-result edge frame computes its edge mask once and
    reuses it on later collects: a repeated masked `degree` over a 10M-edge scan,
    0.19s to 0.03s (#181).
  - Graph columns on a node attribute frame are attached by a dense-id gather
    instead of a hash join and a re-sort into table order: degree onto a
    1M-node table, 0.10s to 0.02s (#182). A float `group_by().agg(mean)` over
    such a frame now sums in table order; it previously followed the join's
    emit order, which varied in the last bits between runs.
  - Queries and scans reuse their DataFusion session, and scans of one object
    store with the same `storage_options` reuse one client and its connection
    pool. Local Parquet footers and statistics are cached across collects and
    revalidated by size and modification time, so a file rewritten in place is
    read afresh (#183).
  - Weighted closeness sums only the nodes each source reaches, so sparse
    graphs no longer pay O(n²): 80k nodes, 1.27s to 0.23s. `avg_path_length`
    and `diameter` reuse per-thread BFS scratch: about 12× faster on sparse
    graphs. Betweenness reduces its partial sums in bounded windows, so exact
    runs no longer hold O(sources/64 × n) memory; small samples run their
    sources in parallel, and unweighted Brandes reads predecessors from the
    in-adjacency (#184).
  - Building the index over compact integer ids (`0..n`, or any dense range, in
    any order) interns through a direct table instead of a hash map: about 1.4×
    faster in memory and up to 1.7× from a Parquet scan, with a smaller id map.
    Sparse ids switch to the hash map automatically. Id lookups over columns with
    no nulls skip per-row null checks (#186).

### Fixed

- `hop`, `shortest_path` and `random_walk` over a filtered edge frame ignored the
  filter: `ur.hop(e.filter(ur.col("w") > 5), 2)` reached nodes through edges
  the filter removed. They now follow only the kept edges, and a graph op over
  such a traversal result (`ur.degree(ur.hop(e.filter(...), 1).from_(s))`)
  stays inside the filtered subgraph (#192).
- A graph op over a traversal of a reshaped scan
  (`ur.pagerank(ur.hop(ur.scan_edges(p).sample(100), 1).from_([1]))`) ran over
  the whole file's index; it now raises the same `NotImplementedError` as the
  in-memory equivalent (#190).
- Graph columns on a 0-row in-memory node frame return an empty frame with the
  table's columns plus the computed ones, instead of raising "No field named
  __ursa_gid" (#153, #182).

### Security

- Bumped `rustls-webpki` 0.103.15 → 0.103.16 (RUSTSEC-2026-0382) and `tokio`
  1.53.1 → 1.54.1 (RUSTSEC-2026-0386). RUSTSEC-2026-0391/0398/0403/0410 in
  `zstd` 0.13 are waived in `deny.toml`: the fixes need zstd 0.14, which arrow,
  parquet and DataFusion 54 do not accept yet, and none of the affected code is
  reached (#188).

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
