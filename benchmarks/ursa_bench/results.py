"""The result-row schema and its Parquet read/append.

One :class:`ResultRow` is emitted per measured *cell* — a single
(library, algorithm, dataset) combination. Rows are the atomic, append-only
record of a run; every downstream artefact (leaderboards, plots, regression
diffs) is derived purely from a table of these.

Design notes:

* **Failures are data, not gaps in the table.** When a library cannot run an
  algorithm we still emit a row with ``status="unsupported"`` (or ``"error"`` /
  ``"incorrect"``). Those rows are the whole point of the flywheel: an
  ``unsupported`` cell is a functionality gap, a slow ``ok`` cell is a perf gap.
  Silently skipping would hide exactly what we want surfaced.
* **Two clocks, always separated.** ``ingest_s`` + ``compute_cold_s`` is the
  user-facing cold end-to-end; ``compute_warm_median_s`` is the kernel-only bar.
* **Provenance travels with every row** — git SHA, machine, library version,
  thread count — so results from different boxes/commits never get silently
  compared as if equal.
"""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field, fields
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

# Bump when the ResultRow columns change in a way old readers must notice.
SCHEMA_VERSION = 2

# The status of a measured cell. `ok` alone is a scored comparison; the rest are
# the flywheel's signal.
STATUS_OK = "ok"  # ran and (if checkable) agreed with the reference
STATUS_UNSUPPORTED = "unsupported"  # the library has no such algorithm — a feature gap
STATUS_INCORRECT = "incorrect"  # ran but disagreed with the reference — a correctness bug
STATUS_ERROR = "error"  # blew up (OOM, exception, timeout) — see `error`


@dataclass
class ResultRow:
    """A single measured cell. See module docstring for the philosophy."""

    # --- provenance -----------------------------------------------------------
    schema_version: int
    run_id: str  # groups all rows from one invocation of `run`
    timestamp: str  # ISO-8601, UTC; stamped by the runner
    git_sha: str
    machine: str  # platform.node()
    os: str  # "Linux" / "Darwin" / ...
    cpu_count: int

    # --- what was measured ----------------------------------------------------
    library: str  # "ursa" | "networkx" | "rustworkx" | ...
    library_version: str
    algorithm: str  # canonical name from algorithms.REGISTRY
    params: str  # JSON of the algorithm params actually used
    threads: int  # thread cap the cell ran under (1 = apples-to-apples)

    dataset: str  # catalog name, e.g. "ba-100k" or "snap-web-google"
    dataset_kind: str  # "synthetic" | "real"
    n_nodes: int
    n_edges: int
    directed: bool
    # structural metadata, recorded so results correlate with graph shape
    density: float  # n_edges / (n_nodes * (n_nodes - 1)) for the directed edge list
    avg_degree: float  # total (in+out) degree averaged over nodes = 2*n_edges/n_nodes
    max_degree: int  # the single busiest node's total degree (skew indicator)

    # --- measurement config ---------------------------------------------------
    iters: int  # timed warm iterations
    warmup: int  # discarded warm iterations

    # --- the numbers (seconds; NaN when not measured, e.g. a failed cell) ------
    ingest_s: float  # Parquet edge list -> native handle
    compute_cold_s: float  # first compute, no warmup (carries lazy build for Ursa)
    compute_warm_median_s: float
    compute_warm_min_s: float
    compute_warm_std_s: float
    peak_rss_bytes: int  # peak RSS of the isolated child process (-1 if unknown)
    baseline_rss_bytes: int  # RSS at child start (before ingest); peak-baseline ~ graph+compute mem

    # --- outcome --------------------------------------------------------------
    status: str  # one of STATUS_*
    correct: bool | None  # None = not checkable (no reference for this cell)
    correctness_detail: str  # human-readable agreement note or mismatch reason
    error: str  # empty unless status in {error}; the exception/text

    # convenience derived field, stored so reports need no recomputation
    end_to_end_cold_s: float = field(default=float("nan"))

    def __post_init__(self) -> None:
        # Derive the cold end-to-end once, here, so it can never drift from its parts.
        if self.status == STATUS_OK or self.status == STATUS_INCORRECT:
            self.end_to_end_cold_s = self.ingest_s + self.compute_cold_s


# Arrow schema, derived to stay in lockstep with the dataclass field order/types.
_PA_TYPES: dict[str, pa.DataType] = {
    "schema_version": pa.int32(),
    "run_id": pa.string(),
    "timestamp": pa.string(),
    "git_sha": pa.string(),
    "machine": pa.string(),
    "os": pa.string(),
    "cpu_count": pa.int32(),
    "library": pa.string(),
    "library_version": pa.string(),
    "algorithm": pa.string(),
    "params": pa.string(),
    "threads": pa.int32(),
    "dataset": pa.string(),
    "dataset_kind": pa.string(),
    "n_nodes": pa.int64(),
    "n_edges": pa.int64(),
    "directed": pa.bool_(),
    "density": pa.float64(),
    "avg_degree": pa.float64(),
    "max_degree": pa.int64(),
    "iters": pa.int32(),
    "warmup": pa.int32(),
    "ingest_s": pa.float64(),
    "compute_cold_s": pa.float64(),
    "compute_warm_median_s": pa.float64(),
    "compute_warm_min_s": pa.float64(),
    "compute_warm_std_s": pa.float64(),
    "peak_rss_bytes": pa.int64(),
    "baseline_rss_bytes": pa.int64(),
    "status": pa.string(),
    "correct": pa.bool_(),
    "correctness_detail": pa.string(),
    "error": pa.string(),
    "end_to_end_cold_s": pa.float64(),
}


def arrow_schema() -> pa.Schema:
    """The Arrow schema for a results table (field order == ResultRow order)."""
    order = [f.name for f in fields(ResultRow)]
    return pa.schema([(name, _PA_TYPES[name]) for name in order])


def rows_to_table(rows: list[ResultRow]) -> pa.Table:
    """Turn result rows into an Arrow table under the canonical schema."""
    schema = arrow_schema()
    cols = {name: [] for name in schema.names}
    for r in rows:
        d = asdict(r)
        for name in schema.names:
            cols[name].append(d[name])
    return pa.table({name: cols[name] for name in schema.names}, schema=schema)


def write_results(rows: list[ResultRow], path: str | Path) -> Path:
    """Append rows to a results Parquet, preserving any existing rows.

    Parquet has no true append; we read-concat-rewrite. Result files are small
    (one row per cell), so this is cheap and keeps the file a single source of
    truth. Schema mismatches across versions are surfaced loudly rather than
    silently coerced.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    new = rows_to_table(rows)
    if path.exists():
        existing = pq.read_table(path)
        if existing.schema.names != new.schema.names:
            raise ValueError(
                f"results schema mismatch at {path}: existing columns differ from "
                f"the current schema (v{SCHEMA_VERSION}). Point --out at a fresh file."
            )
        new = pa.concat_tables([existing.cast(new.schema), new])
    pq.write_table(new, path)
    return path


def read_results(path: str | Path) -> pa.Table:
    """Read a results Parquet back as an Arrow table."""
    return pq.read_table(Path(path))


def params_json(params: dict) -> str:
    """Canonical, stable JSON for a params dict (sorted keys)."""
    return json.dumps(params, sort_keys=True, separators=(",", ":"))
