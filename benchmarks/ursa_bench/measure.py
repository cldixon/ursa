"""Subprocess-isolated measurement of one cell.

Each (library, algorithm, dataset) cell is measured in its **own process**. That
isolation buys three things at once:

* **Honest peak memory** — the child reports its own high-water RSS
  (``getrusage(RUSAGE_SELF).ru_maxrss``, measured at the end of its work), so a
  library's footprint is not polluted by another library imported in the same
  interpreter.
* **No warm-state bleed** — allocator retention, import caches, and lazy globals
  from one library never flatter or penalise the next.
* **A firm thread cap** — thread-count env vars are set in the child's
  environment *before* it imports anything, which is the only point at which
  rayon / OpenMP / BLAS pools read them.

The parent (:func:`measure_cell`) builds the environment, ships a JSON request to
``ursa_bench._child`` over stdin, and parses the JSON reply. All timing and the
RSS reading happen inside the child; the parent only orchestrates.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Every thread-pool knob we know how to pin, so "--threads 1" really is
# single-threaded across rayon (ursa, rustworkx), OpenMP (igraph, later), and the
# BLAS backends behind NetworkX's scipy-powered pagerank.
_THREAD_ENV_VARS = (
    "RAYON_NUM_THREADS",
    "OMP_NUM_THREADS",
    "OPENBLAS_NUM_THREADS",
    "MKL_NUM_THREADS",
    "NUMEXPR_NUM_THREADS",
    "VECLIB_MAXIMUM_THREADS",
)


@dataclass
class CellMeasurement:
    """The raw output of measuring one cell (before it becomes a ResultRow)."""

    status: str  # "ok" | "unsupported" | "error"
    error: str
    ingest_s: float
    compute_cold_s: float
    warm_samples: list[float]  # per-iteration warm compute times
    peak_rss_bytes: int
    baseline_rss_bytes: int  # RSS at child start, before the library/graph is loaded
    result_path: str  # where the child serialised its result (for correctness), or ""


def measure_cell(
    *,
    library: str,
    algorithm: str,
    params: dict,
    dataset_path: str | Path,
    directed: bool,
    iters: int,
    warmup: int,
    threads: int,
    result_out: str | Path | None,
    timeout_s: float,
) -> CellMeasurement:
    """Measure one cell in an isolated subprocess.

    ``result_out`` (when given) is where the child serialises its computed result
    so the parent can cross-check correctness; ``None`` skips that.
    """
    request = {
        "library": library,
        "algorithm": algorithm,
        "params": params,
        "dataset_path": str(dataset_path),
        "directed": directed,
        "iters": iters,
        "warmup": warmup,
        "result_out": str(result_out) if result_out is not None else "",
    }

    env = dict(os.environ)
    for var in _THREAD_ENV_VARS:
        env[var] = str(threads)
    # The child imports ursa_bench.*; make sure the benchmarks/ root is importable
    # regardless of the parent's cwd.
    pkg_root = str(Path(__file__).resolve().parent.parent)
    env["PYTHONPATH"] = pkg_root + os.pathsep + env.get("PYTHONPATH", "")

    try:
        proc = subprocess.run(
            [sys.executable, "-m", "ursa_bench._child"],
            input=json.dumps(request),
            capture_output=True,
            text=True,
            env=env,
            timeout=timeout_s,
        )
    except subprocess.TimeoutExpired:
        return CellMeasurement(
            status="error",
            error=f"timeout after {timeout_s:.0f}s",
            ingest_s=float("nan"),
            compute_cold_s=float("nan"),
            warm_samples=[],
            peak_rss_bytes=-1,
            baseline_rss_bytes=-1,
            result_path="",
        )

    if proc.returncode != 0:
        # A hard crash (OOM-kill, segfault, unhandled panic through the FFI). The
        # child's stderr is the useful bit; keep the tail so the row is diagnosable.
        tail = (proc.stderr or "").strip().splitlines()[-8:]
        return CellMeasurement(
            status="error",
            error=f"exit {proc.returncode}: " + " / ".join(tail),
            ingest_s=float("nan"),
            compute_cold_s=float("nan"),
            warm_samples=[],
            peak_rss_bytes=-1,
            baseline_rss_bytes=-1,
            result_path="",
        )

    # The child prints exactly one JSON object as its final stdout line.
    try:
        payload = json.loads(proc.stdout.strip().splitlines()[-1])
    except (ValueError, IndexError) as exc:
        return CellMeasurement(
            status="error",
            error=f"unparseable child output ({exc}); stderr: {(proc.stderr or '')[:200]}",
            ingest_s=float("nan"),
            compute_cold_s=float("nan"),
            warm_samples=[],
            peak_rss_bytes=-1,
            baseline_rss_bytes=-1,
            result_path="",
        )

    return CellMeasurement(
        status=payload.get("status", "error"),
        error=payload.get("error", ""),
        ingest_s=payload.get("ingest_s", float("nan")),
        compute_cold_s=payload.get("compute_cold_s", float("nan")),
        warm_samples=payload.get("warm_samples", []),
        peak_rss_bytes=payload.get("peak_rss_bytes", -1),
        baseline_rss_bytes=payload.get("baseline_rss_bytes", -1),
        result_path=payload.get("result_path", ""),
    )


def self_peak_rss_bytes() -> int:
    """Peak resident set size of the current process, in bytes.

    ``ru_maxrss`` is kilobytes on Linux and bytes on macOS — normalise to bytes.
    Called from inside the child at the end of its work, so it reflects the true
    high-water mark of ingest + build + compute for that library alone.
    """
    import resource

    maxrss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform == "darwin":
        return int(maxrss)
    return int(maxrss) * 1024
