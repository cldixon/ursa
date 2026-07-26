"""The adapter contract and shared helpers.

An adapter is deliberately tiny: two timed methods and a capability query.

* :meth:`Adapter.ingest` — Parquet edge list → the library's native graph
  handle. This is the *cold ingest* clock: reading the file and building whatever
  structure the library needs before it can compute. (Ursa is lazy here by
  design — its topology is built on first compute — which the two-clock split
  captures honestly.)
* :meth:`Adapter.run` — run one canonical algorithm on a built handle, returning
  a plain ``{original_node_id: value}`` mapping. The *original* id matters: a
  library that assigns its own dense indices (rustworkx) must map back so results
  line up across libraries for the correctness check.
* :meth:`Adapter.supports` — whether this library implements the algorithm at
  all. A ``False`` here becomes an ``unsupported`` result row — the flywheel's
  functionality-gap signal.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

from ..algorithms import Algorithm


class Adapter(ABC):
    #: library name, matching the adapters registry key.
    NAME: str = ""

    @abstractmethod
    def version(self) -> str:
        """Installed version of the library under test."""

    @abstractmethod
    def supports(self, algo: str) -> bool:
        """Whether this library implements ``algo`` (a canonical registry name)."""

    @abstractmethod
    def ingest(self, dataset_path: str | Path, algo: Algorithm):
        """Read the Parquet edge list and build the native graph handle.

        ``algo`` is passed so the adapter can build the right *view* (a directed
        vs undirected structure) the way a real user would for that computation.
        """

    @abstractmethod
    def run(self, handle, algo: Algorithm) -> dict[int, float]:
        """Run ``algo`` on ``handle``; return ``{original_id: value}``."""


def load_edges(dataset_path: str | Path) -> tuple[list[int], list[int]]:
    """Read a Parquet edge list (columns ``src``, ``dst``) as two int lists.

    Lists (not numpy) because that is what NetworkX/rustworkx construction wants;
    the Ursa adapter reads the Arrow table directly instead (see its ingest).
    """
    table = pq.read_table(dataset_path, columns=["src", "dst"])
    src = table.column("src").to_pylist()
    dst = table.column("dst").to_pylist()
    return src, dst


def load_weighted_edges(dataset_path: str | Path) -> tuple[list[int], list[int], list[float]]:
    """Read a Parquet edge list with a ``weight`` column as (src, dst, weight).

    The weight lives in the Parquet, so every library reads the *same* values —
    the only way a weighted cross-library comparison can be apples-to-apples.
    A dataset without a weight column can't back a weighted algorithm.
    """
    table = pq.read_table(dataset_path)
    if "weight" not in table.column_names:
        raise ValueError(
            f"{dataset_path} has no 'weight' column; weighted algorithms need a weighted dataset"
        )
    return (
        table.column("src").to_pylist(),
        table.column("dst").to_pylist(),
        table.column("weight").to_pylist(),
    )


def serialize_result(mapping: dict[int, float], path: str | Path, *, integral: bool) -> None:
    """Write a ``{id: value}`` result to Parquet (columns ``id``, ``value``).

    ``integral`` picks the value type so an int result (triangles, component id)
    round-trips exactly rather than through float.
    """
    ids = list(mapping.keys())
    vals = list(mapping.values())
    value_type = pa.int64() if integral else pa.float64()
    table = pa.table({"id": pa.array(ids, pa.int64()), "value": pa.array(vals, value_type)})
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    pq.write_table(table, path)


def read_result(path: str | Path) -> dict[int, float]:
    """Read back a serialised ``{id: value}`` result."""
    table = pq.read_table(path)
    ids = table.column("id").to_pylist()
    vals = table.column("value").to_pylist()
    return dict(zip(ids, vals, strict=True))
