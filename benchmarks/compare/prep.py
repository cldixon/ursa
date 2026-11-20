"""Prepare one graph size for the comparison: the same input for every engine.

From the Bluesky follows Parquet (or any Parquet with the two endpoint
columns), keep the follows before ``--before`` (all, without it), drop
self-loops and duplicate follows, and write to ``--out``:

- ``edges.parquet``: ``src``, ``dst`` (int64), sorted. Read by both Ursa engines.
- ``dense.npz``: ``src``, ``dst`` as dense int32 indices 0..n-1, and ``ids``
  (int64, the original id of each index). Read by igraph, rustworkx, NetworkX.
  Skipped with ``--no-dense`` (the full graph needs ~100 GB to build it).
- ``meta.json``: node and edge counts, and how long each part took. This time is
  charged to no engine.

Why deduplicate: the data has parallel follows, which NetworkX's ``DiGraph``
would collapse and Ursa would count, so every engine gets the same simple graph.
Usage::

    python prep.py --source ~/Downloads/ICPSR_300499-V1.1/deidentified_follows_edgelist.parquet \\
        --before 2023-06-01 --out /tmp/cmp/bsky-5m
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument(
        "--source", required=True, help="Parquet edge list (a path or glob ursa can read)"
    )
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--before", help="keep rows with date_followed before this date (YYYY-MM-DD)")
    ap.add_argument("--src-col", default="from")
    ap.add_argument("--dst-col", default="to")
    ap.add_argument("--no-dense", action="store_true", help="skip dense.npz (Ursa-only sizes)")
    ap.add_argument("--ursa", default=shutil.which("ursa") or "ursa")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    edges = a.out / "edges.parquet"
    meta: dict = {"source": a.source, "before": a.before or ""}

    where = [f'"{a.src_col}" <> "{a.dst_col}"']
    if a.before:
        where.append(f"date_followed < DATE '{a.before}'")
    sql = (
        f'COPY (SELECT DISTINCT "{a.src_col}" AS src, "{a.dst_col}" AS dst '
        f"FROM '{a.source}' WHERE {' AND '.join(where)} ORDER BY src, dst) "
        f"TO '{edges.as_posix()}'"
    )
    t0 = time.perf_counter()
    subprocess.run(
        [a.ursa, "--no-init", "--quiet", "--format", "csv", "-c", sql],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    meta["edges_s"] = round(time.perf_counter() - t0, 3)

    import pyarrow.parquet as pq

    meta["m"] = pq.ParquetFile(edges).metadata.num_rows
    if not a.no_dense:
        import numpy as np

        t0 = time.perf_counter()
        t = pq.read_table(edges)
        s, d = t.column("src").to_numpy(), t.column("dst").to_numpy()
        ids, inv = np.unique(np.concatenate([s, d]), return_inverse=True)
        if len(ids) >= 2**31:
            sys.exit("prep.py: more than 2^31 nodes; dense ids need int64")
        np.savez(
            a.out / "dense.npz",
            src=inv[: len(s)].astype(np.int32),
            dst=inv[len(s) :].astype(np.int32),
            ids=ids,
        )
        meta["n"] = len(ids)
        meta["dense_s"] = round(time.perf_counter() - t0, 3)
    else:
        out = subprocess.run(
            [
                a.ursa,
                "--no-init",
                "--format",
                "csv",
                "-c",
                f"SELECT count(*) FROM (SELECT src AS id FROM '{edges.as_posix()}' "
                f"UNION SELECT dst FROM '{edges.as_posix()}')",
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        meta["n"] = int(out.stdout.strip().splitlines()[-1])
    (a.out / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(json.dumps(meta))
    return 0


if __name__ == "__main__":
    sys.exit(main())
