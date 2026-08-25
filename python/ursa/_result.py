"""A materialized result — what ``collect()`` returns.

Wraps the Arrow data handed back across the FFI by the Rust execution path. The
engine returns its result as a **list of** ``pyarrow.RecordBatch`` — a chunked
result that is never concatenated into one contiguous batch (#60) — so this holds
a ``pyarrow.Table`` (chunked, zero-copy over those batches). Egress to
polars/pyarrow is zero-copy.
"""

from __future__ import annotations

from typing import Any

#: Rows shown by ``repr()``. The rest are elided behind a ``…`` row — the shape
#: line already carries the true count, so the preview stays cheap.
_REPR_MAX_ROWS = 10

#: Longest rendered cell before it is clipped with an ellipsis, so one wide string
#: column cannot blow the preview past a terminal width.
_REPR_MAX_WIDTH = 24


def _fmt_cell(value: Any) -> str:
    """One table cell as a display string.

    Floats get 6 significant digits (``0.477977``, but ``1e-12`` stays readable);
    nulls read as ``null`` rather than ``None``, matching the dataframe idiom the
    rest of the API borrows from.
    """
    if value is None:
        return "null"
    if isinstance(value, float):
        text = f"{value:.6g}"
    elif isinstance(value, str):
        text = value
    else:
        text = str(value)
    if len(text) > _REPR_MAX_WIDTH:
        return text[: _REPR_MAX_WIDTH - 1] + "…"
    return text


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
        """As a ``polars.DataFrame`` (zero-copy via Arrow).

        ``polars`` is an optional dependency — the error names the extra that
        installs it, since a bare ``ModuleNotFoundError`` here is the first thing
        a new user hits after following any example in the docs (#122).
        """
        try:
            import polars as pl
        except ModuleNotFoundError as exc:  # pragma: no cover - import guard
            raise ModuleNotFoundError(
                "to_polars() requires polars, which is an optional dependency. "
                "Install it with: pip install 'ursa-graph[polars]'"
            ) from exc

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
        """A Polars-shaped preview: shape, column names, dtypes, and the head.

        A repr fires on every REPL evaluation, so only ``_REPR_MAX_ROWS`` rows are
        ever sliced out of the table — formatting a million-row result costs the
        same as formatting ten. Falls back to the terse form if anything about the
        data resists rendering; a repr that raises is worse than a plain one.
        """
        try:
            return self._render()
        except Exception:  # pragma: no cover - repr must never raise
            cols = ", ".join(self.columns)
            return f"<MaterializedFrame rows={self._table.num_rows} cols=[{cols}]>"

    def _render(self) -> str:
        n_rows, n_cols = self._table.num_rows, self._table.num_columns
        shape = f"shape: ({n_rows}, {n_cols})"
        if n_cols == 0:
            return f"{shape}\n(no columns)"

        names = self.columns
        dtypes = [str(field.type) for field in self._table.schema]
        head = self._table.slice(0, _REPR_MAX_ROWS).to_pylist()
        truncated = n_rows > _REPR_MAX_ROWS

        # One column of display strings per field: header, rule, dtype, then cells.
        columns = [
            [name, "---", dtype]
            + [_fmt_cell(row[name]) for row in head]
            + (["…"] if truncated else [])
            for name, dtype in zip(names, dtypes, strict=True)
        ]
        widths = [max(len(cell) for cell in column) for column in columns]

        def line(left: str, fill: str, mid: str, right: str) -> str:
            return left + mid.join(fill * (w + 2) for w in widths) + right

        def row(cells: list[str]) -> str:
            return "│ " + " ┆ ".join(c.ljust(w) for c, w in zip(cells, widths, strict=True)) + " │"

        out = [shape, line("┌", "─", "┬", "┐")]
        out += [row([column[i] for column in columns]) for i in range(3)]
        out.append(line("╞", "═", "╪", "╡"))
        out += [row([column[i] for column in columns]) for i in range(3, len(columns[0]))]
        out.append(line("└", "─", "┴", "┘"))
        return "\n".join(out)
