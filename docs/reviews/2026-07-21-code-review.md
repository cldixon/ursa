# Full-codebase review — 2026-07-21

A deep review of the entire codebase at v0.1.1 (post PR #32), covering
correctness, drift, refactoring opportunities, performance, memory, and unsafe
Rust. Four parallel review passes (ursa-core; ursa-plan; the PyO3 + Python
layer; tests/CI/docs) were cross-verified against the code before filing.
Every finding below with an issue number has a corresponding GitHub issue
containing the full evidence and a concrete fix proposal.

## Headline results

- **Unsafe Rust: none.** Zero `unsafe` blocks across all three crates. The
  crash surface is instead ~101 `unwrap`/`expect`/`panic!` sites, of which a
  handful are reachable at runtime from valid user input (#38, #39) — panics
  cross PyO3 as `PanicException` (a `BaseException`), so these matter.
- **The architecture holds.** The load-bearing spec contracts were positively
  verified: one `collect()` really is one DataFusion `LogicalPlan` with graph
  ops as first-class `UserDefinedLogicalNode`s (no "traffic cop"); every
  ExecutionPlan dispatches kernels via `spawn_blocking` (the rayon-on-tokio
  trap is handled); the GIL is released on all long-running paths (two
  demo-only exceptions, #49); the `edge_ids` permutation, lazy per-direction
  CSR, and dense-u32 interning all match the spec; the topology index is
  genuinely built once and shared (with two contract gaps, #47).
- **The dominant defect class is silent drops, not wrong math.** Kernel math
  spot-checks held up. What shipped broken are steps/parameters that are
  accepted and then quietly ignored (#33, #34) and spec'd surface that can
  never succeed (#35, #36) — all traceable to hand-enumerated dispatch tables
  that new parameters fall through, and to test gaps at exactly those seams
  (#53).
- **Docs lag badly.** The "skeleton"-era narrative survives in `help(ursa)`,
  docs.rs, and module docstrings while the README (mostly) tells the truth
  (#50). `__version__` already drifted from the released version (#51).

## Filed issues

### Correctness (silent wrong results / dead surface)

| # | Finding |
|---|---|
| #33 | `rename` silently ignored in all collectors; `select`/`reverse` silently ignored on traversal frames |
| #34 | Silently ignored parameters: `connected_components(mode=)` (returns weak labels for `mode="strong"`), `neighbors(from_=)`, `scan_*(**format_opts)`, `sink_csv(**opts)`, `describe()` pipeline tails |
| #35 | `EdgeFrame.reverse()` discards the data source — every algorithm on a reversed frame fails; `filter()` likewise nulls the source, contradicting the index-contract table |
| #36 | `read_edges`/`read_nodes` always raise; plain scan→`to_polars()` unsupported |
| #37 | Weighted algorithms on scan-backed frames depend on row-order stability across two independent scans of the same file (glob paths make this a real hazard) |
| #48 | Traversal composition: result frames advertise parent role names but emit literal `src`/`dst` columns; hop-of-hop and filtered-NodeFrame seeds fail |

### Robustness (panics, hardening)

| # | Finding |
|---|---|
| #38 | LargeUtf8 id columns >2 GiB panic in `id_map::as_utf8` (plus a full-column copy even on success) |
| #39 | Panic-surface audit: duplicate/reserved output-column names, `todo!()` in public `expr::lower`, batch-assembly `expect`s, inconsistent weighted-kernel validation, silent u32 truncation guards, misleading string-weight error, exception taxonomy |
| #40 | Determinism gaps: Louvain epsilon tie-breaks over HashMap iteration order; rayon f64 reductions vary run-to-run; sampled betweenness ignores the spec'd `seed=` and uses a layout-biased strided sample |

### Performance & memory

| # | Finding |
|---|---|
| #41 | A new multi-thread tokio Runtime built and torn down per collect (7 sites; 2–3× per weighted/scan collect); fresh SessionContext per collect |
| #42 | `neighbor_agg`: `n_unique` is O(d²) per node via `Vec::contains`; whole kernel sequential |
| #43 | Undirected sorted adjacency rebuilt per call as `Vec<Vec<u32>>` (triangle/clustering); Louvain level-0 graph built with one HashMap per node |
| #44 | Scans collect+concat the full table (~2× peak); no node-scan projection or predicate pushdown; id column re-copied per collect; result re-concatenated |
| #45 | `Topology` retains `src_dense`/`dst_dense` forever (8 B/edge); string interning double-allocates every distinct id |
| #46 | Parallelism backlog: serial CSR build, serial `k_hop` seeds, serialized `random_walk` RNG stream, Brandes per-source allocation churn, serial stats BFS, per-iteration dangling scan |
| #47 | Python layer: node-attr tables re-scanned from disk every collect; module-global index-build lock serializes unrelated frames; derived frames don't share the index memo |

### Structure, docs, packaging, CI, tests

| # | Finding |
|---|---|
| #49 | Dead code & duplication sweep: `logical.rs` stub nodes (name-colliding with the real ones), dead `ursa_session`, always-true `is_executable`, `adjacency(Both)` trap, demo namespace (incl. two GIL-held kernels), 4-way `query.rs` copy-paste (node query already drifted: missing `distinct`), duplicated `visit_neighbors`, weighted/unweighted skeleton copy-paste (closeness pair already diverged), logical-node Eq/Hash inconsistencies |
| #50 | Stale-docs sweep: "skeleton" narrative in `__init__.py`/`_frames.py`/both crates' `lib.rs`, weighted-louvain and weighted-path contradictions, README self-contradiction, `pip install ursa` vs `ursa-graph`, undocumented multiplicity semantics |
| #51 | `pyarrow` missing from runtime dependencies (fresh install is broken); `__version__` hard-coded and already drifted; `pyo3/extension-module` hard-enabled breaks `cargo test -p ursa-py` |
| #52 | Release publishes wheels no job ever imports; CI tests one Python on one OS (macOS/Windows first compiled during release) |
| #53 | Coverage backlog: string ids for 7 of 13 verbs, NetworkX cross-checks, concurrency test for the shared index build, Python-boundary error paths, parameter plumbing, glob scans |

## Pre-existing issues affected

- **#14, #15, #16** (betweenness, label_propagation, louvain) describe work
  that has since shipped with tests — candidates to close.
- **#18** (relational surface) is the "implement for real" counterpart of #33's
  "stop silently dropping" fix.
- **#9** (`store=`) and **#8** (cloud integration tests) already cover two
  review findings; #50 and #53 reference rather than duplicate them.

## Minor items not worth their own issue

Recorded here so they aren't lost; fold into adjacent PRs opportunistically:

- `_frames.py` `join()` doesn't record the `other` frame in its plan step —
  latent (join is unwired) but will bite whoever wires it (fold into #18).
- `_execute.py:496-499` accesses the scan batch positionally
  (`batch.column(0/1)`) — one refactor away from silently swapping direction;
  use field names.
- "Zero-copy" docstrings in `_io.py` are unqualified, but the ingress prep
  casts non-int64 ids and concatenates multi-chunk columns (real copies);
  qualify as "zero-copy when already contiguous int64/utf8". The FFI boundary
  itself is genuinely zero-copy.
- `_Frame.collect() -> Self` is a knowingly false type contract patched with
  `# ty: ignore`; declare `-> MaterializedFrame` on the base.
- Weighted closeness/betweenness zero-weight-edge semantics (distance-0
  neighbors excluded; equal-cost paths through 0-weight edges can undercount
  sigma) — document strictly-positive weights, or reject zeros (noted in #39/#49).
- `scan.rs` `Url::parse` misroutes Windows drive paths (`C:\...` parses as
  scheme `c`) into the unsupported-scheme error.
- Filter values are carried as `f64` — i64 filter literals beyond 2^53 lose
  precision.
- Empty (zero-row) edge/node files error with the same message as a failed
  read (noted in #44).

## What's solid (for calibration)

Kernel unit coverage in ursa-core is thorough and value-asserting (including
weighted variants, empty graphs, determinism seeds, self-loop handling);
`label_propagation`/`random_walk` determinism is genuinely order-independent;
ursa-plan's weight seam enforces non-negativity and the `edge_ids` gather
contract; the Python e2e tests assert values; the double-checked index-build
locking is correct as written; and the crate boundaries (`ursa-core` free of
DataFusion, the dialect quarantined at one seam) have held through ~30 PRs of
multi-session development.
