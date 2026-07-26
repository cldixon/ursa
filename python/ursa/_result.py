"""A materialized result — what ``collect()`` returns.

Wraps the Arrow data handed back across the FFI by the Rust execution path. The
engine returns its result as a **list of** ``pyarrow.RecordBatch`` — a chunked
result that is never concatenated into one contiguous batch (#60) — so this holds
a ``pyarrow.Table`` (chunked, zero-copy over those batches). Egress to
polars/pyarrow is zero-copy.
"""

from __future__ import annotations

from typing import Any


class MaterializedFrame:
    """An eagerly-computed frame backed by a chunked Arrow ``Table``.

    Accepts whatever the execution path produced: a **list** of RecordBatches (the
    native query egress), a single RecordBatch (the plain-collect pyarrow paths), or
    an already-assembled ``pyarrow.Table`` — normalized to one chunked Table so no
    batch is ever copied to make the result contiguous.
    """

    __slots__ = ("_table",)

    def __init__(self, data: Any) -> None:
        import pyarrow as pa

        if isinstance(data, pa.Table):
            self._table = data
        elif isinstance(data, list):
            # The native egress: a (non-empty, schema-carrying) batch list.
            self._table = pa.Table.from_batches(data)
        else:  # a single pyarrow.RecordBatch
            self._table = pa.Table.from_batches([data])

    def to_arrow(self) -> Any:
        """As a ``pyarrow.Table`` (zero-copy)."""
        return self._table

    def to_polars(self) -> Any:
        """As a ``polars.DataFrame`` (zero-copy via Arrow)."""
        import polars as pl

        return pl.from_arrow(self._table)

    def to_dicts(self) -> list[dict[str, Any]]:
        """As a list of row dicts."""
        return self._table.to_pylist()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        """Write the result to a Parquet file. ``opts`` pass through to pyarrow."""
        import pyarrow.parquet as pq

        pq.write_table(self._table, path, **opts)

    def sink_csv(self, path: str) -> None:
        """Write the result to a CSV file."""
        import pyarrow.csv as pacsv

        pacsv.write_csv(self._table, path)

    @property
    def columns(self) -> list[str]:
        return self._table.schema.names

    def __len__(self) -> int:
        return self._table.num_rows

    def __repr__(self) -> str:
        cols = ", ".join(self.columns)
        return f"<MaterializedFrame rows={self._table.num_rows} cols=[{cols}]>"
