"""Fast, seeded R-MAT edge lists as Parquet, for the SQL scale test.

R-MAT (the Graph500 Kronecker generator) places each edge by descending
``scale`` levels of the adjacency matrix, choosing one of four quadrants per
level with probabilities ``a, b, c, d``. The result is a directed graph with a
power-law degree distribution, multi-edges and some self-loops, as Graph500's
is; nothing is deduplicated. Generation is vectorised numpy: one uniform draw
per edge per level, so 20M edges take seconds, not the NetworkX minutes.

Node labels are randomly permuted (so id order says nothing about degree), and
``scale`` is chosen so there are about ``edges / edge_factor`` node slots; many
low-probability slots get no edge, so the realised node count is lower.

Each size writes, under ``--out``:

- ``rmat-<size>-int.parquet``: ``src``, ``dst`` as ``int64``;
- ``rmat-<size>-w.parquet``: the same edges plus a ``weight`` (``float64`` in
  ``[0.01, 1)``), with ``--weighted``;
- ``rmat-<size>-str.parquet``: the same edges with ``string`` ids ``"v<id>"``,
  for sizes up to ``--string-max``.

The same ``--seed`` and size always give the same files, whatever other sizes
are generated alongside.

Usage::

    python benchmarks/sql_scale/gen.py --out DIR --sizes 1M,5M,20M --weighted
    python benchmarks/sql_scale/gen.py --help
"""

from __future__ import annotations

import argparse
import math
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

# Graph500's quadrant probabilities (d = 1 - a - b - c = 0.05).
A, B, C = 0.57, 0.19, 0.19


def parse_size(text: str) -> int:
    """``"20M"`` → 20_000_000; ``"500K"`` → 500_000; plain integers as given."""
    text = text.strip().upper()
    mult = {"K": 1_000, "M": 1_000_000, "G": 1_000_000_000}.get(text[-1:], 1)
    digits = text[:-1] if mult != 1 else text
    return int(float(digits) * mult)


def size_label(edges: int) -> str:
    for unit, div in (("g", 10**9), ("m", 10**6), ("k", 10**3)):
        if edges % div == 0:
            return f"{edges // div}{unit}"
    return str(edges)


def rmat(edges: int, scale: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """``edges`` R-MAT edges over ``2**scale`` node slots, labels permuted."""
    rng = np.random.default_rng([seed, edges, scale])
    src = np.zeros(edges, dtype=np.int64)
    dst = np.zeros(edges, dtype=np.int64)
    ab, abc = A + B, A + B + C
    r = np.empty(edges, dtype=np.float32)
    for _ in range(scale):
        rng.random(out=r, dtype=np.float32)
        # Quadrants c and d set the source bit; b and d set the destination's.
        high = r >= ab
        src <<= 1
        src |= high
        dst <<= 1
        dst |= ((r >= A) & ~high) | (r >= abc)
    perm = rng.permutation(np.int64(1) << np.int64(scale))
    return perm[src], perm[dst]


def write(table: pa.Table, path: Path) -> None:
    pq.write_table(table, path, compression="zstd", row_group_size=1 << 20)


def generate(
    edges: int,
    out: Path,
    *,
    seed: int,
    edge_factor: int,
    weighted: bool,
    strings: bool,
) -> list[Path]:
    scale = max(1, round(math.log2(max(edges // edge_factor, 2))))
    label = size_label(edges)
    t0 = time.perf_counter()
    src, dst = rmat(edges, scale, seed)
    written = []
    ints = pa.table({"src": src, "dst": dst})
    path = out / f"rmat-{label}-int.parquet"
    write(ints, path)
    written.append(path)
    if weighted:
        rng = np.random.default_rng([seed, edges, scale, 1])
        weight = rng.uniform(0.01, 1.0, edges)
        path = out / f"rmat-{label}-w.parquet"
        write(ints.append_column("weight", pa.array(weight)), path)
        written.append(path)
    if strings:
        as_str = [
            pc.binary_join_element_wise("v", pc.cast(col, pa.string()), "")
            for col in (ints["src"], ints["dst"])
        ]
        path = out / f"rmat-{label}-str.parquet"
        write(pa.table({"src": as_str[0], "dst": as_str[1]}), path)
        written.append(path)
    seen = np.zeros(1 << scale, dtype=bool)
    seen[src] = True
    seen[dst] = True
    nodes = int(seen.sum())
    secs = time.perf_counter() - t0
    print(
        f"{label}: {edges:,} edges, {nodes:,} nodes (scale {scale}), {secs:.1f}s",
        file=sys.stderr,
    )
    return written


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Generate seeded R-MAT (Graph500-style) edge lists as Parquet.",
    )
    parser.add_argument("--out", type=Path, required=True, help="output directory")
    parser.add_argument(
        "--sizes",
        default="1M,5M,20M",
        help="comma-separated edge counts, e.g. 1M,5M,20M (default: %(default)s)",
    )
    parser.add_argument("--seed", type=int, default=42, help="default: %(default)s")
    parser.add_argument(
        "--edge-factor",
        type=int,
        default=16,
        help="edges per node slot; sets the scale (default: %(default)s, as Graph500)",
    )
    parser.add_argument(
        "--weighted", action="store_true", help="also write a variant with a weight column"
    )
    parser.add_argument(
        "--string-max",
        default="5M",
        help="write string-id variants for sizes up to this (default: %(default)s; 0 for none)",
    )
    args = parser.parse_args(argv)
    args.out.mkdir(parents=True, exist_ok=True)
    string_max = parse_size(args.string_max)
    for size in args.sizes.split(","):
        edges = parse_size(size)
        for path in generate(
            edges,
            args.out,
            seed=args.seed,
            edge_factor=args.edge_factor,
            weighted=args.weighted,
            strings=edges <= string_max,
        ):
            print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
