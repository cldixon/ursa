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
from .adapters import get_adapter, load_edges, read_result
from .measure import CellMeasurement, measure_cell
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
    rounds: int = 1  # interleaved measurement rounds (see run_matrix); 1 = no interleaving
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


def _rotate(libs: list[str], k: int) -> list[str]:
    """Cyclically rotate the library order by ``k`` positions.

    Rotating per round means no library is pinned to the same measurement slot,
    so an order-correlated bias (a warm cache, a mid-pass slowdown) is spread
    across all of them rather than always favouring or penalising the same one.
    """
    if not libs:
        return libs
    k %= len(libs)
    return libs[k:] + libs[:k]


def _pool(measurements: list[CellMeasurement]) -> CellMeasurement:
    """Pool one cell's measurements across interleaved rounds into a single record.

    Warm samples are concatenated (the median/min later runs over the whole pool,
    so it draws on every time window, not one). ingest and cold take the *min*
    across rounds — the least-contended observation is the truest "how fast can
    this go". Peak RSS takes the max. Errors are deterministic, so a cell that
    fails one round fails them all; the first result is propagated.
    """
    ok = [m for m in measurements if m.status == "ok"]
    if not ok:
        return measurements[0]
    warm = [s for m in ok for s in m.warm_samples]
    result_path = next((m.result_path for m in ok if m.result_path), "")
    return CellMeasurement(
        status="ok",
        error="",
        ingest_s=min(m.ingest_s for m in ok),
        compute_cold_s=min(m.compute_cold_s for m in ok),
        warm_samples=warm,
        peak_rss_bytes=max(m.peak_rss_bytes for m in ok),
        result_path=result_path,
    )


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

            # Community-detection correctness scores modularity, which needs the
            # edge list. Load it once per dataset, only when actually required.
            mod_edges = None
            if want_result and any(
                algorithms.get(a).compare == "modularity" for a in cfg.algorithms
            ):
                src, dst = load_edges(path)
                mod_edges = list(zip(src, dst, strict=True))

            ds_rows: list[ResultRow] = []
            for algo_name in cfg.algorithms:
                algo = algorithms.get(algo_name)

                # --- measure every requested library, interleaved over rounds --
                # Each round measures every cell once, rotating the library order
                # (see _rotate). Samples pool across the time-separated rounds
                # (see _pool), so a mid-run host drift is shared fairly rather
                # than skewing whichever library happened to run next to it.
                per_lib: dict[str, list] = {lib: [] for lib in cfg.libraries}
                for rnd in range(cfg.rounds):
                    for lib in _rotate(cfg.libraries, rnd):
                        if not get_adapter(lib).supports(algo_name):
                            continue
                        # Capture the result only the first time we measure a lib.
                        result_out = (
                            tmpdir / f"{ds_name}.{algo_name}.{lib}.parquet"
                            if want_result and not per_lib[lib]
                            else None
                        )
                        per_lib[lib].append(
                            measure_cell(
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
                        )

                measured: dict[str, tuple] = {}
                for lib in cfg.libraries:
                    if not per_lib[lib]:  # never measured -> unsupported
                        measured[lib] = (None, None)
                        continue
                    pooled = _pool(per_lib[lib])
                    res = None
                    if (
                        pooled.status == STATUS_OK
                        and pooled.result_path
                        and Path(pooled.result_path).exists()
                    ):
                        res = read_result(pooled.result_path)
                    measured[lib] = (pooled, res)

                # --- reference for correctness --------------------------------
                reference = _reference(measured, algo, path, want_result)

                # --- build rows -----------------------------------------------
                for lib in cfg.libraries:
                    ds_rows.append(
                        _build_row(
                            prov,
                            cfg,
                            spec,
                            n_nodes,
                            n_edges,
                            algo,
                            lib,
                            measured[lib],
                            reference,
                            mod_edges,
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


def _build_row(
    prov, cfg, spec, n_nodes, n_edges, algo, lib, measured, reference, mod_edges=None
) -> ResultRow:
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
        correct, detail = correctness.compare(reference, res, algo, mod_edges)
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
