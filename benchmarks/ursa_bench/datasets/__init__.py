"""The dataset catalog: named specs that materialise to a Parquet edge list.

A :class:`DatasetSpec` knows how to *produce* its Parquet (generate a synthetic
graph, or download+parse a real one) and caches the result under a cache dir, so
asking for a dataset twice is free the second time. The runner reads the
materialised Parquet both to feed the adapters and to record the graph's shape
(node/edge counts) as provenance on every result row.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import pyarrow.parquet as pq

from . import real, synthetic

DEFAULT_CACHE = Path(__file__).resolve().parent.parent.parent / "data"

# The tiny graph CI runs the harness smoke against (kept small enough to generate
# and race in seconds).
SMOKE = "er-smoke"


@dataclass
class DatasetSpec:
    name: str
    kind: str  # "synthetic" | "real"
    directed: bool
    _build: Callable[[Path], Path]  # (parquet_path) -> parquet_path
    note: str = ""

    def materialize(self, cache_dir: str | Path = DEFAULT_CACHE) -> Path:
        path = Path(cache_dir) / self.kind / f"{self.name}.parquet"
        if path.exists():
            return path
        return self._build(path)


def _catalog() -> dict[str, DatasetSpec]:
    specs: dict[str, DatasetSpec] = {}
    seed = 42

    def syn(name: str, fn: Callable[[Path], Path], note: str) -> None:
        specs[name] = DatasetSpec(name, "synthetic", True, fn, note)

    # CI smoke — tiny, deterministic.
    syn(SMOKE, lambda p: synthetic.er(200, 6.0, seed, p), "tiny ER (CI smoke)")

    # Size sweeps for asymptotic curves. avg degree ~10 (ER), m=5 (BA).
    for n, label in ((1_000, "1k"), (10_000, "10k"), (100_000, "100k")):
        syn(
            f"er-{label}",
            lambda p, n=n: synthetic.er(n, 10.0, seed, p),
            f"Erdős–Rényi n={n}, avg-deg~10",
        )
        syn(
            f"ba-{label}",
            lambda p, n=n: synthetic.ba(n, 5, seed, p),
            f"Barabási–Albert n={n}, m=5",
        )
    syn("grid-128", lambda p: synthetic.grid(128, p), "128x128 lattice (16,384 nodes)")

    # Real graphs (never run in CI; downloaded + cached on first use).
    specs["snap-email-eu-core"] = DatasetSpec(
        "snap-email-eu-core",
        "real",
        True,
        lambda p: real.snap_edge_list(
            "https://snap.stanford.edu/data/email-Eu-core.txt.gz",
            p,
            cache_dir=DEFAULT_CACHE,
        ),
        "SNAP email-Eu-core (1,005 nodes, ~25k edges)",
    )
    specs["snap-web-google"] = DatasetSpec(
        "snap-web-google",
        "real",
        True,
        lambda p: real.snap_edge_list(
            "https://snap.stanford.edu/data/web-Google.txt.gz",
            p,
            cache_dir=DEFAULT_CACHE,
        ),
        "SNAP web-Google (~875k nodes, ~5.1M edges)",
    )
    return specs


CATALOG = _catalog()


def resolve(name: str) -> DatasetSpec:
    try:
        return CATALOG[name]
    except KeyError:
        raise KeyError(f"unknown dataset {name!r}; known: {', '.join(sorted(CATALOG))}") from None


def names() -> list[str]:
    return list(CATALOG)


def graph_shape(parquet_path: str | Path) -> tuple[int, int]:
    """(n_nodes, n_edges) for a materialised edge list."""
    table = pq.read_table(parquet_path, columns=["src", "dst"])
    n_edges = table.num_rows
    nodes = set(table.column("src").to_pylist()) | set(table.column("dst").to_pylist())
    return len(nodes), n_edges
