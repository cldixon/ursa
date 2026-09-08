# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The distribution is `ursa-graph`; the import name is `ursa`.

## [Unreleased]

## [0.3.0] — 2026-08-29

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
- Documentation site at <https://ursa.cldixon.dev>, including a semantics
  reference that pins each kernel to the NetworkX call it is checked against.

### Changed

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

[Unreleased]: https://github.com/cldixon/ursa/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/cldixon/ursa/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/cldixon/ursa/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/cldixon/ursa/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cldixon/ursa/releases/tag/v0.1.0
