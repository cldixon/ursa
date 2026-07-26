# Ursa benchmarks — the flywheel

A cross-library graph-benchmark harness built as an **iterative flywheel**: every
run mechanically emits a to-do list — **perf gaps** ("X is 8× slower than
rustworkx on Y") and **functionality gaps** ("Ursa has no strong components") —
that feed back into the issue tracker. The coverage matrix (algorithm × library ×
dataset) *is* the roadmap.

This is a separate, opt-in package. A plain `uv sync` does **not** install it;
the comparison libraries live in their own dependency group so the core dev
environment stays lean.

```bash
uv sync --group bench          # installs typer, rich, rustworkx, igraph, networkx

# from the repo root:
uv run --group bench python benchmarks --help
```

## The idea in one picture

```
generate ─▶ run ──────────────▶ results.parquet ──▶ report ─▶ leaderboard
(datasets)  (measure, isolated)  (raw rows only)     (read)    + gaps to-do
                                       │
                                       └── diff across commits, plot, re-render …
```

The **measurement** (`run`) and the **analysis** (`report`) are deliberately
separate commands over a Parquet file. A run does exactly one thing: measure and
write raw rows. Everything downstream reads that Parquet — nothing is welded into
the measurement loop, so the same results can be re-rendered, diffed, or fed to
new tooling without re-running anything.

## What it measures, and why it's honest

Ursa's real advantage is not out-running a mature C/Rust core on raw kernels —
it's **getting from an Arrow/Parquet table to an answer without building a
`Graph` object**. Every other library pays a conversion tax to ingest your
tabular data. So two clocks are always separated:

- **cold end-to-end** (`--metric cold`) — from the Parquet edge list to the
  result: ingest + build + compute. The user-facing number, and where Ursa's
  design shows.
- **warm kernel** (`--metric warm`) — the algorithm on an already-built handle.
  The kernel-vs-kernel bar, isolated from ingest.

Plus **graph memory** (`--metric mem`) and **memory/edge** (`--metric memedge`)
and **ingest** (`--metric ingest`). Memory is *isolated*: the child records its
RSS floor before the library-under-test loads, so `peak − baseline` strips the
interpreter/import baseline and leaves the graph + compute footprint. That number
(and bytes/edge — a CSR vs a dict-of-dicts) is only meaningful at **large** graphs;
below ~10⁵ edges it's dominated by import cost, so small/negative deltas render as
`—`.

### End-to-end pipelines, not just kernels

Single kernels are the *competition's* home turf. Ursa's thesis is **composition**,
so the suite also races a realistic composed query as one workload:

- **`pipeline_influencers`** — top-K nodes by PageRank among those with in-degree ≥ k.
  Ursa runs it as **one** `collect()` (`nodes().with_columns(pagerank, in_degree)
  .filter(…).sort(…).head(K)`); NetworkX/rustworkx/igraph build the graph, compute,
  then filter/sort/top in Python — the tax Ursa fuses away. Correctness compares the
  selected top-K set (allowing minor boundary churn from PageRank ties).

Every measured cell runs in its **own subprocess**, so a library's peak memory
and warm state are never polluted by another library imported in the same
interpreter, and the thread cap (`--threads`, default `1` for apples-to-apples)
is pinned before any library initialises its pools.

**Interleaving for fairness under drift** (`--rounds`, default `1`). Within a
cell, the timed iterations run back-to-back; but with `--rounds N` the *whole
matrix* is measured N times, **rotating the library order each round** and pooling
each cell's samples across the rounds. So no library is pinned to the same slot or
time window, and a mid-run host slowdown is shared across all of them rather than
skewing whichever library happened to run next to it. (Repetitions cut *variance*,
not *bias* — reducing with median/min over the pooled samples, never mean, is what
keeps a right-skewed timing distribution honest. Systematic host bias needs this
interleaving plus, ideally, dedicated hardware — see issue #80.)

**Parallelism is its own dimension — and fair.** `--threads` can be *swept*
(`-t 1 -t 2 -t 4`), and every library is given the **same** budget at each level,
with *all* the relevant knobs set (`RAYON_NUM_THREADS` for Ursa/rustworkx,
`OMP_NUM_THREADS`, and the BLAS vars `OPENBLAS`/`MKL`/… so NetworkX's scipy-backed
kernels can use threads too). Those that parallelise speed up; those that don't —
igraph's C core (single-threaded in the standard wheel) and NetworkX under the GIL
— stay flat. That's a true property of each library, measured generously, not a
thumb on the scale. Single-thread stays the apples-to-apples baseline; the sweep
shows what more cores buy. The thread count is recorded on every row.

Every cell is also **correctness-checked** against the NetworkX reference (the
oracle), using the exact definitional alignments pinned in
`tests/test_networkx_reference.py` (out-edge closeness, raw betweenness,
undirected triangles, …). A fast wrong answer is recorded as `incorrect`, not
scored — as loud a signal as a slow one.

**Failures are data.** A library that lacks an algorithm produces an
`unsupported` row; a crash/OOM produces an `error` row. Those cells are the whole
point — they're the gaps the flywheel surfaces.

### Comparison libraries

**NetworkX** (pure-Python oracle / floor), **rustworkx** (a mature Rust core), and
**igraph** (a mature C core). Each library's per-algorithm conventions are pinned
to the canonical definition and *verified empirically by the correctness gate* —
so a convention slip shows up as an `incorrect` row, never a silent pass. Two that
were caught this way: rustworkx closeness needs the graph **reversed** (it measures
incoming distance), and igraph closeness is `mode="out", normalized=True`; both then
match Ursa's out-edge form exactly. Neither rustworkx nor igraph exposes a per-node
triangle count, so `triangle_count` is `unsupported` for both. Community detection
(`louvain`, `label_propagation`) is scored by **modularity**, not by labels — a
heuristic partition passes if its Q is within a small slack of the oracle's.

## Commands

```bash
# Inspect what can be raced
uv run --group bench python benchmarks libraries      # + installed versions
uv run --group bench python benchmarks algorithms     # canonical defs
uv run --group bench python benchmarks datasets        # the catalog

# Materialise a dataset (synthetic build or real download), cached under data/
uv run --group bench python benchmarks generate ba-10k

# Measure a slice of the matrix (filters compose)
uv run --group bench python benchmarks run \
    -l ursa -l rustworkx -a pagerank -a triangle_count \
    -d er-1k -d ba-10k --iters 5 --threads 1

# Render a leaderboard + the gaps to-do list from the results Parquet
uv run --group bench python benchmarks report --metric cold
uv run --group bench python benchmarks report --metric warm --markdown  # committable

# The tiny end-to-end run CI uses (asserts Ursa runs + agrees with the oracle)
uv run --group bench python benchmarks smoke
```

Example warm-kernel leaderboard (single-thread, er-1k; `n/a` = library lacks the
algo). No single library wins everything — igraph leads the centralities, Ursa
leads triangle_count and louvain, rustworkx leads pagerank:

| algorithm | ursa | igraph | networkx | rustworkx |
|---|---|---|---|---|
| pagerank | 2.8ms | 2.0ms | 35.2ms | **1.5ms** |
| betweenness | 177ms | **103ms** | 3.40s | 213ms |
| closeness | **38ms** | 41ms | 564ms | 140ms |
| triangle_count | **4.6ms** | n/a | 14.1ms | n/a |
| clustering_coefficient | 5.9ms | **0.6ms** | 69.9ms | n/a |
| louvain | **15.8ms** | 28.5ms | 243ms | n/a |
| connected_components | 1.8ms | **0.1ms** | 0.5ms | 0.3ms |

## Datasets

Two kinds, both needed:

- **Synthetic** (seeded, reproducible) — Erdős–Rényi, Barabási–Albert
  (power-law), grid, and **stochastic block model** (`sbm-*`, planted communities —
  the honest test for Louvain / label propagation, which have a ground-truth
  partition to recover rather than a lucky modularity), swept across sizes so an
  accidental O(d²) shows up as a bending curve before a user hits it.
- **Real** (SNAP edge lists) — real degree skew and credibility; downloaded,
  checksummed, and cached as Parquet on first use. Never run in CI.

Every result row carries the graph's **structural metadata** — `density`,
`avg_degree`, `max_degree` — so a win can be correlated with graph *shape* (skew,
density) instead of just a dataset name.

Materialised Parquet lands under `benchmarks/data/` (git-ignored — reproducible
from the catalog). Raw results land under `benchmarks/results/` (git-ignored —
machine-specific; commit curated Markdown leaderboards, not per-box Parquet).

## Layout

| Module | Role |
|---|---|
| `results.py` | the result-row schema + Parquet append/read |
| `measure.py` + `_child.py` | subprocess-isolated timing + peak-RSS capture |
| `datasets/` | seeded synthetic generators + real-graph loaders → Parquet |
| `adapters/` | one per library: `ingest(path)→handle`, `run(handle, algo)→result` |
| `algorithms.py` | the canonical algorithm registry (params + reference definition) |
| `correctness.py` | cross-check a result against the oracle, within tolerance |
| `runner.py` | drive the matrix into result rows |
| `report.py` | read a results Parquet → rich tables + a Markdown leaderboard |
| `cli.py` | the `typer` + `rich` front door |

## Extending

- **A library** — add an adapter under `adapters/` implementing `version`,
  `supports`, `ingest`, `run`, and register it in `adapters/__init__.py`.
- **An algorithm** — add an entry to `algorithms.REGISTRY` (with its reference
  definition + comparison mode), teach each adapter's `run` to produce it, and add
  the comparison mode to `correctness.py` if it's new.
- **A dataset** — add a `DatasetSpec` to the catalog in `datasets/__init__.py`.

## Caveats (read before quoting a number)

- **Tiny graphs flatter no one honestly.** At a few hundred nodes, fixed
  per-call overhead (Ursa's `collect()` builds a DataFusion plan; NetworkX has
  Python-call overhead) dominates the kernel, so ratios there measure overhead,
  not algorithms. Trust the *curve* across the size sweep, not a single small
  point.
- **Memory only bites at scale.** At ≤10⁴ nodes, peak RSS is dominated by the
  interpreter + library import baseline (~100 MB), not the graph. The CSR-vs-dict
  memory story needs millions of edges to show.
- **Convergence tolerance.** Timing uses a realistic fixed iteration budget for
  PageRank, so three implementations converge to *slightly* different points; the
  correctness tolerance accommodates that drift while still catching real bugs.
