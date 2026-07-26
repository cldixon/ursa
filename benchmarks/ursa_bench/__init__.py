"""ursa_bench — the cross-library graph-benchmark flywheel.

A run does exactly one thing: **measure** and write every raw number to Parquet
(``results.py``). Everything downstream — leaderboards, plots, regression logic —
is a *separate* step that reads that Parquet (``report.py``). No analysis is welded
into the measurement loop, so the same results file can be re-rendered, diffed
across commits, or fed to new tooling without re-running anything.

The honest question this harness asks is not "is Ursa's kernel faster than a mature
C/Rust core" but "how fast does a user get from an Arrow/Parquet table to an answer"
— so every library pays its real ingest tax, and two clocks are always separated:

* **cold** — from the Parquet edge list to the result (ingest + build + compute);
* **warm** — the algorithm on an already-built handle (kernel-only).

Layout:

* ``results``    — the result-row schema + Parquet append/read.
* ``measure``    — subprocess-isolated timing + peak-RSS capture (via ``wait4``).
* ``datasets``   — seeded synthetic generators + real-graph loaders, all → Parquet.
* ``adapters``   — one per library: ingest(path)→handle, run(handle, algo)→result.
* ``algorithms`` — the canonical algorithm registry (params + reference definition).
* ``correctness``— cross-check a result against the NetworkX reference, within tol.
* ``runner``     — drive the (library × algorithm × dataset) matrix into result rows.
* ``report``     — read a results Parquet → rich tables + a Markdown leaderboard.
* ``cli``        — the ``typer`` + ``rich`` front door.
"""

from __future__ import annotations

__all__ = ["__version__"]

# The harness versions itself independently of the library under test, so an old
# results Parquet can be recognised even as the schema evolves (see results.SCHEMA_VERSION).
__version__ = "0.1.0"
