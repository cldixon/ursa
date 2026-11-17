"""Write a small synthetic stand-in for the Bluesky files, for smoke tests.

The files have the real names and columns (``deidentified_follows_edgelist.parquet``:
``from``, ``to``, ``date_followed``; ``deidentified_nodes.csv.gz``: ``id``,
``date-created``, ``active``, ``status``), and the node ids run.py's traversal
and pattern steps start from, so every step runs end to end in seconds. The
values are not the real graph's, so run.py checks nothing unless
``--before 2024-01-01``, and the smoke test runs without it.

Usage::

    python3 smoke_data.py DIR [--nodes 50000] [--edges 1000000]
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

SPECIAL = (6987779, 33127650, 7078889, 25003023, 36081159)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("dir", type=Path)
    ap.add_argument("--nodes", type=int, default=50_000)
    ap.add_argument("--edges", type=int, default=1_000_000)
    ap.add_argument("--ursa", default=shutil.which("ursa") or "ursa")
    a = ap.parse_args()
    a.dir.mkdir(parents=True, exist_ok=True)
    ids = (
        f"(SELECT CAST(x AS BIGINT) AS id FROM generate_series(0, {a.nodes - 1}) t(x) "
        + "".join(f"UNION ALL SELECT CAST({i} AS BIGINT) " for i in SPECIAL)
        + ")"
    )
    n = a.nodes + len(SPECIAL)
    # Endpoint i maps onto the id list by position, so every edge joins two real ids.
    sql = f"""
CREATE VIEW ids AS SELECT id, row_number() OVER (ORDER BY id) - 1 AS pos FROM {ids};
COPY (SELECT id, DATE '2023-01-01' + CAST(pos % 600 AS INT) AS "date-created",
             pos % 50 <> 0 AS active,
             CASE WHEN pos % 50 = 0 THEN 'deactivated' ELSE 'normal' END AS status
      FROM ids ORDER BY pos)
  TO '{(a.dir / "deidentified_nodes.csv.gz").as_posix()}'
  STORED AS CSV OPTIONS ('format.compression' 'gzip');
COPY (SELECT s.id AS "from", d.id AS "to",
             DATE '2023-06-01' + CAST(e.x % 400 AS INT) AS date_followed
      FROM (SELECT x, CAST((x * 2654435761) % {n} AS BIGINT) AS sp,
                      CAST(((x * 40503) + (x % 7) * 97) % {n} AS BIGINT) AS dp
            FROM generate_series(0, {a.edges - 1}) t(x)) e
      JOIN ids s ON s.pos = e.sp JOIN ids d ON d.pos = e.dp
      ORDER BY e.x)
  TO '{(a.dir / "deidentified_follows_edgelist.parquet").as_posix()}';
"""
    subprocess.run([a.ursa, "--no-init", "-c", sql], check=True, stdout=subprocess.DEVNULL)
    print(f"wrote the smoke data to {a.dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
