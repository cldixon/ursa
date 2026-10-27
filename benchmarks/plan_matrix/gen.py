"""Write the plan-matrix datasets (seeded, about 470 MB of Parquet).

    python gen.py DATA_DIR

Uniform random edge lists with an `f64` weight column `w`, at the sizes the scenarios
use, plus a sparse-id copy of the 10M graph (ids scattered over i64) and a shuffled
1M-row node attribute table.
"""

from __future__ import annotations

import os
import sys

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

out = sys.argv[1] if len(sys.argv) > 1 else "data"
os.makedirs(out, exist_ok=True)
rng = np.random.default_rng(7)


def save(name: str, n: int, m: int, compact: bool = True) -> None:
    s, d = rng.integers(0, n, m), rng.integers(0, n, m)
    if not compact:
        ids = rng.integers(-(2**62), 2**62, n)
        s, d = ids[s], ids[d]
    pq.write_table(pa.table({"s": s, "d": d, "w": rng.random(m)}), f"{out}/{name}.parquet")


save("e10m", 1_000_000, 10_000_000)
save("e10m_sparse", 1_000_000, 10_000_000, compact=False)
save("e1m", 200_000, 1_000_000)
save("e500k", 100_000, 500_000)
save("e80k", 80_000, 80_000)
save("e50k", 50_000, 50_000)
ids = np.arange(1_000_000)
rng.shuffle(ids)
pq.write_table(pa.table({"id": ids, "x": rng.random(1_000_000)}), f"{out}/nodes1m.parquet")
