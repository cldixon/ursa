"""The measurement child — runs one cell and reports timings + peak RSS as JSON.

Invoked as ``python -m ursa_bench._child`` by :func:`ursa_bench.measure.measure_cell`,
with the request on stdin. It imports exactly one library adapter, so its peak
RSS reflects that library alone. Its last stdout line is a single JSON object;
anything a library prints to stdout is tolerated because only that final line is
parsed.

Sequence: ingest (cold) → one cold compute → ``warmup`` discarded computes →
``iters`` timed computes. The cold compute is timed separately because for a lazy
engine (Ursa) it carries the one-off topology build that warm runs then reuse.
"""

from __future__ import annotations

import dataclasses
import json
import sys
import time
import traceback


def _fail(status: str, error: str) -> None:
    print(json.dumps({"status": status, "error": error}))
    sys.exit(0)


def main() -> None:
    req = json.loads(sys.stdin.read())

    from ursa_bench import algorithms
    from ursa_bench.adapters import get_adapter, serialize_result
    from ursa_bench.measure import self_peak_rss_bytes

    # RSS before the library-under-test is imported or any graph is built — the
    # interpreter + bench + pyarrow floor. peak - baseline isolates the memory the
    # graph and computation actually added (bytes/edge at scale).
    baseline_rss = self_peak_rss_bytes()

    library = req["library"]
    algo_name = req["algorithm"]
    result_out = req.get("result_out") or ""

    try:
        base = algorithms.get(algo_name)
    except KeyError as exc:
        _fail("error", str(exc))

    # Overlay any caller-supplied params onto the canonical defaults.
    params = {**base.params, **req.get("params", {})}
    algo = dataclasses.replace(base, params=params)

    try:
        adapter = get_adapter(library)
    except KeyError as exc:
        _fail("error", str(exc))

    if not adapter.supports(algo_name):
        # Not a failure — a functionality gap. Recorded as its own status.
        _fail("unsupported", f"{library} has no {algo_name}")

    try:
        t0 = time.perf_counter()
        handle = adapter.ingest(req["dataset_path"], algo)
        ingest_s = time.perf_counter() - t0

        t0 = time.perf_counter()
        result = adapter.run(handle, algo)
        compute_cold_s = time.perf_counter() - t0

        for _ in range(int(req.get("warmup", 0))):
            adapter.run(handle, algo)

        warm_samples: list[float] = []
        for _ in range(int(req.get("iters", 1))):
            t0 = time.perf_counter()
            result = adapter.run(handle, algo)
            warm_samples.append(time.perf_counter() - t0)

        if result_out:
            integral = base.compare in {"int_map", "partition", "topk"}
            serialize_result(result, result_out, integral=integral)
    except Exception:
        _fail("error", "".join(traceback.format_exc().splitlines(keepends=True)[-6:]))

    print(
        json.dumps(
            {
                "status": "ok",
                "error": "",
                "ingest_s": ingest_s,
                "compute_cold_s": compute_cold_s,
                "warm_samples": warm_samples,
                "peak_rss_bytes": self_peak_rss_bytes(),
                "baseline_rss_bytes": baseline_rss,
                "result_path": result_out,
            }
        )
    )


if __name__ == "__main__":
    main()
