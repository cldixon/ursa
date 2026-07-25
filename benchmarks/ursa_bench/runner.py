"""Drive the (library × algorithm × dataset) matrix into result rows.

For each dataset the runner materialises the Parquet once, records its shape, and
then for each algorithm measures every requested library in its own subprocess.
The NetworkX result (when the graph is small enough for the oracle) is the
reference every other library's result is checked against.

Results are written to the output Parquet **per dataset**, so a long run that is
interrupted still leaves everything measured so far on disk — the Parquet-first
contract means those rows are already a usable, re-renderable artefact.
"""

from __future__ import annotations

import os
import platform
import statistics
import subprocess
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from . import algorithms, correctness, datasets
from .adapters import get_adapter, read_result
from .measure import measure_cell
from .results import (
    SCHEMA_VERSION,
    STATUS_INCORRECT,
    STATUS_OK,
    ResultRow,
    params_json,
    write_results,
)


@dataclass
class RunConfig:
    libraries: list[str]
    algorithms: list[str]
    datasets: list[str]
    iters: int = 5
    warmup: int = 1
    threads: int = 1
    reference_max_nodes: int = 50_000  # above this the oracle is too slow; correctness = None
    timeout_s: float = 600.0
    cache_dir: Path = datasets.DEFAULT_CACHE


def _git_sha() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True, stderr=subprocess.DEVNULL
        ).strip()
    except Exception:
        return "unknown"


def _provenance(run_id: str) -> dict:
    return {
        "schema_version": SCHEMA_VERSION,
        "run_id": run_id,
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "git_sha": _git_sha(),
        "machine": platform.node(),
        "os": platform.system(),
        "cpu_count": os.cpu_count() or 0,
    }


def _warm_stats(samples: list[float]) -> tuple[float, float, float]:
    if not samples:
        nan = float("nan")
        return nan, nan, nan
    median = statistics.median(samples)
    smallest = min(samples)
    std = statistics.pstdev(samples) if len(samples) > 1 else 0.0
    return median, smallest, std


def run_matrix(cfg: RunConfig, out_path: str | Path, run_id: str) -> list[ResultRow]:
    """Execute the matrix, writing rows to ``out_path`` per dataset. Returns all rows."""
    prov = _provenance(run_id)
    all_rows: list[ResultRow] = []

    with tempfile.TemporaryDirectory(prefix="ursa-bench-") as tmp:
        tmpdir = Path(tmp)
        for ds_name in cfg.datasets:
            spec = datasets.resolve(ds_name)
            path = spec.materialize(cfg.cache_dir)
            n_nodes, n_edges = datasets.graph_shape(path)
            want_result = n_nodes <= cfg.reference_max_nodes

            ds_rows: list[ResultRow] = []
            for algo_name in cfg.algorithms:
                algo = algorithms.get(algo_name)

                # --- measure every requested library ---------------------------
                measured: dict[str, tuple] = {}
                for lib in cfg.libraries:
                    adapter = get_adapter(lib)
                    if not adapter.supports(algo_name):
                        measured[lib] = (None, None)  # -> unsupported row below
                        continue
                    result_out = (
                        tmpdir / f"{ds_name}.{algo_name}.{lib}.parquet" if want_result else None
                    )
                    m = measure_cell(
                        library=lib,
                        algorithm=algo_name,
                        params=algo.params,
                        dataset_path=path,
                        directed=spec.directed,
                        iters=cfg.iters,
                        warmup=cfg.warmup,
                        threads=cfg.threads,
                        result_out=result_out,
                        timeout_s=cfg.timeout_s,
                    )
                    res = None
                    if m.status == STATUS_OK and m.result_path and Path(m.result_path).exists():
                        res = read_result(m.result_path)
                    measured[lib] = (m, res)

                # --- reference for correctness --------------------------------
                reference = _reference(measured, algo, path, want_result)

                # --- build rows -----------------------------------------------
                for lib in cfg.libraries:
                    ds_rows.append(
                        _build_row(
                            prov, cfg, spec, n_nodes, n_edges, algo, lib, measured[lib], reference
                        )
                    )

            write_results(ds_rows, out_path)
            all_rows.extend(ds_rows)

    return all_rows


def _reference(measured: dict, algo, path, want_result: bool):
    """The reference result for correctness, or None when not checkable."""
    if not want_result:
        return None
    # Reuse NetworkX's measured result if it was in the run and succeeded.
    if "networkx" in measured and measured["networkx"][1] is not None:
        return measured["networkx"][1]
    # Otherwise compute the oracle untimed (parent process; small graphs only).
    nx_adapter = get_adapter("networkx")
    if not nx_adapter.supports(algo.name):
        return None
    try:
        return nx_adapter.run(nx_adapter.ingest(path, algo), algo)
    except Exception:
        return None


def _build_row(prov, cfg, spec, n_nodes, n_edges, algo, lib, measured, reference) -> ResultRow:
    m, res = measured
    common = dict(
        **prov,
        library=lib,
        library_version=_safe_version(lib),
        algorithm=algo.name,
        params=params_json(algo.params),
        threads=cfg.threads,
        dataset=spec.name,
        dataset_kind=spec.kind,
        n_nodes=n_nodes,
        n_edges=n_edges,
        directed=spec.directed,
        iters=cfg.iters,
        warmup=cfg.warmup,
    )
    nan = float("nan")

    if m is None:  # library doesn't implement this algorithm
        return ResultRow(
            **common,
            ingest_s=nan,
            compute_cold_s=nan,
            compute_warm_median_s=nan,
            compute_warm_min_s=nan,
            compute_warm_std_s=nan,
            peak_rss_bytes=-1,
            status="unsupported",
            correct=None,
            correctness_detail="",
            error=f"{lib} does not implement {algo.name}",
        )

    if m.status != STATUS_OK:
        return ResultRow(
            **common,
            ingest_s=nan,
            compute_cold_s=nan,
            compute_warm_median_s=nan,
            compute_warm_min_s=nan,
            compute_warm_std_s=nan,
            peak_rss_bytes=m.peak_rss_bytes,
            status=m.status,
            correct=None,
            correctness_detail="",
            error=m.error,
        )

    median, smallest, std = _warm_stats(m.warm_samples)
    if res is not None:
        correct, detail = correctness.compare(reference, res, algo)
    else:
        correct, detail = None, "result not captured (graph above reference size)"
    status = STATUS_INCORRECT if correct is False else STATUS_OK

    return ResultRow(
        **common,
        ingest_s=m.ingest_s,
        compute_cold_s=m.compute_cold_s,
        compute_warm_median_s=median,
        compute_warm_min_s=smallest,
        compute_warm_std_s=std,
        peak_rss_bytes=m.peak_rss_bytes,
        status=status,
        correct=correct,
        correctness_detail=detail,
        error="",
    )


def _safe_version(lib: str) -> str:
    try:
        return get_adapter(lib).version()
    except Exception:
        return "unknown"
