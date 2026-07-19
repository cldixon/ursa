"""A materialized result — what ``collect()`` returns.

Wraps the pyarrow ``RecordBatch`` handed back across the FFI by the Rust
execution path. Everything is Arrow, so egress to polars/pyarrow is zero-copy.
"""

from __future__ import annotations

from typing import Any


class MaterializedFrame:
    """An eagerly-computed ``(id, value)`` frame backed by an Arrow batch."""

    __slots__ = ("_batch",)

    def __init__(self, batch: Any) -> None:
        self._batch = batch  # pyarrow.RecordBatch

    def to_arrow(self) -> Any:
        """As a ``pyarrow.Table`` (zero-copy)."""
        import pyarrow as pa

        return pa.Table.from_batches([self._batch])

    def to_polars(self) -> Any:
        """As a ``polars.DataFrame`` (zero-copy via Arrow)."""
        import polars as pl

        return pl.from_arrow(self.to_arrow())

    def to_dicts(self) -> list[dict[str, Any]]:
        """As a list of row dicts."""
        return self.to_arrow().to_pylist()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        """Write the result to a Parquet file. ``opts`` pass through to pyarrow."""
        import pyarrow.parquet as pq

        pq.write_table(self.to_arrow(), path, **opts)

    def sink_csv(self, path: str) -> None:
        """Write the result to a CSV file."""
        import pyarrow.csv as pacsv

        pacsv.write_csv(self.to_arrow(), path)

    @property
    def columns(self) -> list[str]:
        return self._batch.schema.names

    def __len__(self) -> int:
        return self._batch.num_rows

    def __repr__(self) -> str:
        cols = ", ".join(self.columns)
        return f"<MaterializedFrame rows={self._batch.num_rows} cols=[{cols}]>"
