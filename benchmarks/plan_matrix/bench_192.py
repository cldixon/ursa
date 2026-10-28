"""Targeted timings for #192 (masks and plain scans in Rust), one per process.

    python bench_192.py DATA_DIR CASE

Cases:
- `traversal_mask`: PageRank over a traversal result at 10M edges. The first
  collect builds the traversal's mask; a repeat reuses it, so the difference is
  the mask construction.
- `masked_degree_first`: the first masked `degree` over a 10M-edge scan (the
  plan matrix's `masked_degree_scan` times the repeat).
- `scan_pushdown`: a 1%-selective filter on a scan of a 10M-row file sorted by
  the filtered column, in 128K-row groups (written on first use).

Prints `case<TAB>name=seconds ...`.
"""

from __future__ import annotations

import os
import sys
import time

import numpy as np
import pyarrow.parquet as pq

import ursa as ur

DATA, CASE = sys.argv[1], sys.argv[2]


def timed(f):
    t = time.perf_counter()
    f()
    return time.perf_counter() - t


def best(f, k=3):
    return min(timed(f) for _ in range(k))


def traversal_mask():
    e = ur.from_arrow(pq.read_table(f"{DATA}/e10m.parquet"), src="s", dst="d")
    ur.degree(e).collect()  # the index, built before anything is timed
    seeds = np.random.default_rng(3).integers(0, 1_000_000, 2_000).tolist()
    view = ur.hop(e, n=2, direction="both").from_(seeds)
    first = timed(lambda: ur.pagerank(view).collect())
    repeat = best(lambda: ur.pagerank(view).collect())
    return {"first": first, "repeat": repeat, "mask": first - repeat}


def masked_degree_first():
    e = ur.scan_edges(f"{DATA}/e10m.parquet", src="s", dst="d")
    ur.degree(e).collect()
    f = e.filter(ur.col("w") > 0.5)
    return {"first": timed(lambda: ur.degree(f).collect())}


def scan_pushdown():
    path = f"{DATA}/e10m_sorted.parquet"
    if not os.path.exists(path):
        t = pq.read_table(f"{DATA}/e10m.parquet").sort_by("s")
        pq.write_table(t, path, row_group_size=1 << 17)
    scan = ur.scan_edges(path, src="s", dst="d")
    pred = ur.col("s") < 10_000
    rows = scan.filter(pred).collect().to_arrow().num_rows
    return {"filter_collect": best(lambda: scan.filter(pred).collect()), "rows": rows}


out = {
    "traversal_mask": traversal_mask,
    "masked_degree_first": masked_degree_first,
    "scan_pushdown": scan_pushdown,
}[CASE]()
print(CASE + "\t" + " ".join(f"{k}={v:.6g}" for k, v in out.items()), flush=True)
