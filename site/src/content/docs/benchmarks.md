---
title: Benchmarks
description: The benchmark flywheel — what it measures, why the two clocks are separated, and why failures are recorded as data rather than omitted.
subtitle: No numbers are published here yet. The methodology is, so that the numbers mean something when they are.
---

Ursa ships a cross-library benchmark harness in `benchmarks/`, built as an **iterative flywheel**:
every run mechanically emits a to-do list — perf gaps ("*X is 8× slower than rustworkx on Y*") and
functionality gaps ("*Ursa has no strong components*") — that feed back into the issue tracker.
The coverage matrix (algorithm × library × dataset) *is* the roadmap.

It is a separate, opt-in package. A plain `uv sync` does not install it.

```bash
uv sync --group bench     # typer, rich, rustworkx, igraph, networkx
uv run --group bench python benchmarks --help
```

## The shape

```
generate ─▶ run ──────────────▶ results.parquet ──▶ report ─▶ leaderboard
(datasets)  (measure, isolated)  (raw rows only)     (read)    + gaps to-do
                                       │
                                       └── diff across commits, plot, re-render …
```

Measurement and analysis are deliberately separate commands over a Parquet file. A run does
exactly one thing: measure, and write raw rows. Everything downstream reads that Parquet, so the
same results can be re-rendered, diffed or fed to new tooling without re-running anything.

## Two clocks, always separated

Ursa's real advantage is not out-running a mature C or Rust core on a raw kernel. It is **getting
from an Arrow/Parquet table to an answer without building a `Graph` object** — every other library
pays a conversion tax to ingest tabular data. Reporting one number would hide exactly the thing
worth measuring, so there are two:

- **cold end-to-end** — from the Parquet edge list to the result: ingest + build + compute. The
  user-facing number, and where the design shows.
- **warm kernel** — the algorithm on an already-built handle. The kernel-versus-kernel bar,
  isolated from ingest.

Plus **graph memory**, **bytes per edge**, and **ingest** on their own. Memory is *isolated*: the
child process records its RSS floor before the library under test loads, so `peak − baseline`
strips the interpreter and import baseline. That number is only meaningful at large graphs — below
about 10⁵ edges it is dominated by import cost, and the report renders it as `—` rather than
pretending otherwise.

## Composition, not just kernels

Single kernels are the competition's home turf. Ursa's thesis is composition, so the suite also
races a realistic composed query as one workload: `pipeline_influencers`, the top-*K* nodes by
PageRank among those with in-degree ≥ *k*. Ursa runs it as one `collect()`; the others build the
graph, compute, then filter, sort and top-*k* in Python — the tax Ursa fuses away.

## Why it is fair

**Isolation.** Every measured cell runs in its own subprocess, so one library's peak memory and
warm state never pollute another's, and the thread cap is pinned before any library initialises
its pools.

**Interleaving.** With `--rounds N` the whole matrix is measured N times, **rotating the library
order each round** and pooling each cell's samples. No library is pinned to the same slot or time
window, so a mid-run host slowdown is shared rather than landing on whoever ran next to it.

**Median and min, never mean.** Repetitions cut *variance*, not *bias*. A right-skewed timing
distribution has an outlier-sensitive mean, so the reducers are median and min.

**Parallelism as its own dimension.** `--threads` can be swept, and every library gets the same
budget at each level with *all* the relevant knobs set — `RAYON_NUM_THREADS`, `OMP_NUM_THREADS`,
and the BLAS variables so NetworkX's scipy-backed kernels can use threads too. Those that
parallelise speed up; those that do not stay flat. That is a true property of each library,
measured generously. Single-thread remains the apples-to-apples baseline; the sweep shows what
more cores buy. The thread count is recorded on every row.

**Correctness gates the number.** Every cell is checked against the NetworkX oracle using the
exact definitional alignments in [Semantics](/docs/semantics). A fast wrong answer is recorded as
`incorrect`, not scored — as loud a signal as a slow one.

## Failures are data

A library that lacks an algorithm produces an `unsupported` row. A crash or OOM produces an
`error` row. Those cells are the point: they are the gaps the flywheel surfaces, in both
directions. Where rustworkx weights only PageRank, weighted closeness and betweenness show up as
honest `unsupported` cells rather than quietly missing columns.

Every result row carries provenance — git SHA, machine, CPU count, library versions — so runs from
different hardware are never silently compared or trended as equal.

## Why there is no leaderboard on this page

Because the numbers would not yet mean what a reader would take them to mean. The harness
currently runs on developer hardware, where the parallelism sweep caps at four cores and the host
is shared. Publishing absolute timings from that is precisely the kind of claim the methodology
above exists to avoid.

The work to fix it is scoped: a pinned container image and a reproducible runner on dedicated
hardware, so a published number comes with an image, a machine class and an exact command that
anyone can re-run. Comparative results are already robust to host noise — every library runs on
the same box in the same run — so what is missing is the authority of the *absolute* numbers, and
that is worth waiting for.

Until then: `uv run --group bench python benchmarks --help`, and run it on your own data. That is
the number that actually applies to you.
