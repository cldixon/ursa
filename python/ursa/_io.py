"""Ingress: scan/read/from constructors.

The primary in-memory constructors are ``EdgeFrame(data, src=, dst=)`` and
``NodeFrame(data, id=)`` (see ``_frames.py``): they accept native Python data —
row dicts, a dict of columns, a polars / pandas DataFrame, or a pyarrow
Table/RecordBatch — normalized to one canonical Arrow source by
``_to_arrow_table`` so the user never touches pyarrow directly. ``from_arrow`` /
``from_polars`` / ``from_pandas`` here are thin, typed aliases over those
constructors.

``scan_*`` is lazy (returns a frame that is a plan); ``read_*`` is the eager
convenience (scan + collect). Object storage is first-class: ``s3://``, ``gs://``,
``az://`` and globs are accepted. Edge scans push their column projection
(``src``/``dst`` plus any weight columns) into Parquet; node-column projection and
predicate pushdown are not yet wired.

However data arrives — scan/read from Parquet, the in-memory constructors, or the
aliases — once it is inside an EdgeFrame/NodeFrame nothing downstream can tell how
it was built; execution is identical from that point on.

Constructors build correctly-typed lazy frames with the role mapping recorded (so
``.src_col`` etc. and ``.explain()`` work eagerly); the file read runs in the
engine at ``collect()``.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, overload

from ._frames import EdgeFrame, NodeFrame, _PlanStep

if TYPE_CHECKING:
    from ._result import MaterializedFrame


def _reject_format_opts(format_opts: dict[str, Any]) -> None:
    """Format options (e.g. a CSV ``delimiter=``) are not plumbed into the scan
    yet, so accepting and dropping them would silently ignore the caller. Raise
    instead until they are threaded through to the Rust scan."""
    if format_opts:
        keys = ", ".join(sorted(format_opts))
        raise NotImplementedError(
            f"scan format options ({keys}) are not supported yet; they would be "
            "silently ignored. Only Parquet/CSV with default parsing is wired."
        )


def scan_edges(
    path: str | list[str],
    *,
    src: str,
    dst: str,
    storage_options: dict[str, Any] | None = None,
    store: Any | None = None,
    **format_opts: Any,
) -> EdgeFrame:
    """Lazily scan an edge list from Parquet/CSV (local or object storage).

    ``src`` / ``dst`` are role mappings, not renames. ``store`` accepts a
    pre-configured ``obstore`` store in place of ``storage_options`` (both bind
    the same underlying Rust ``object_store`` crate).
    """
    _reject_format_opts(format_opts)
    step = _PlanStep(
        "scan_edges",
        {
            "path": path,
            "src": src,
            "dst": dst,
            "storage_options": storage_options,
            "store": store,
            "format_opts": format_opts,
        },
    )
    return EdgeFrame._construct(
        src_col=src,
        dst_col=dst,
        plan=(step,),
        scan={
            "path": path,
            "src": src,
            "dst": dst,
            "storage_options": storage_options,
            "store": store,
        },
    )


def scan_nodes(
    path: str | list[str],
    *,
    id: str,
    storage_options: dict[str, Any] | None = None,
    store: Any | None = None,
    **format_opts: Any,
) -> NodeFrame:
    """Lazily scan a node/attribute table; ``id`` is the id-role mapping."""
    _reject_format_opts(format_opts)
    step = _PlanStep(
        "scan_nodes",
        {
            "path": path,
            "id": id,
            "storage_options": storage_options,
            "store": store,
            "format_opts": format_opts,
        },
    )
    return NodeFrame._construct(
        id_col=id,
        plan=(step,),
        scan={"path": path, "id": id, "storage_options": storage_options, "store": store},
    )


def read_edges(path: str | list[str], **kwargs: Any) -> MaterializedFrame:
    """Eager convenience: ``scan_edges(...).collect()``."""
    return scan_edges(path, **kwargs).collect()


def read_nodes(path: str | list[str], **kwargs: Any) -> MaterializedFrame:
    """Eager convenience: ``scan_nodes(...).collect()``."""
    return scan_nodes(path, **kwargs).collect()


@overload
def from_polars(df: Any, *, src: str, dst: str) -> EdgeFrame: ...
@overload
def from_polars(df: Any, *, id: str) -> NodeFrame: ...
def from_polars(
    df: Any, *, src: str | None = None, dst: str | None = None, id: str | None = None
) -> EdgeFrame | NodeFrame:
    """Build a frame from an in-memory ``polars.DataFrame``, zero-copy via Arrow.

    A thin, typed alias for the primary constructors ``EdgeFrame(df, src=, dst=)``
    / ``NodeFrame(df, id=)``. Pass ``src``/``dst`` for an EdgeFrame or ``id`` for a
    NodeFrame.
    """
    return _from_inmemory(df, src, dst, id)


@overload
def from_arrow(tbl: Any, *, src: str, dst: str) -> EdgeFrame: ...
@overload
def from_arrow(tbl: Any, *, id: str) -> NodeFrame: ...
def from_arrow(
    tbl: Any, *, src: str | None = None, dst: str | None = None, id: str | None = None
) -> EdgeFrame | NodeFrame:
    """Build a frame from a ``pyarrow.Table`` / ``RecordBatch``, zero-copy.

    A thin, typed alias for ``EdgeFrame(tbl, src=, dst=)`` / ``NodeFrame(tbl, id=)``.
    """
    return _from_inmemory(tbl, src, dst, id)


@overload
def from_pandas(df: Any, *, src: str, dst: str) -> EdgeFrame: ...
@overload
def from_pandas(df: Any, *, id: str) -> NodeFrame: ...
def from_pandas(
    df: Any, *, src: str | None = None, dst: str | None = None, id: str | None = None
) -> EdgeFrame | NodeFrame:
    """Build a frame from an in-memory ``pandas.DataFrame`` via Arrow.

    A thin, typed alias for ``EdgeFrame(df, src=, dst=)`` / ``NodeFrame(df, id=)``.
    The pandas index is dropped (``preserve_index=False``) so it never leaks in as
    a stray column. pandas is detected at runtime and never imported by Ursa unless
    the caller already has it.
    """
    return _from_inmemory(df, src, dst, id)


def _from_inmemory(data: Any, src, dst, id):
    """Dispatch an in-memory input to the right primary constructor. The public
    ``from_arrow`` / ``from_polars`` / ``from_pandas`` aliases all funnel here; the
    constructors normalize any supported flavour to one canonical Arrow source."""
    if src is not None and dst is not None:
        return EdgeFrame(data, src=src, dst=dst)
    if id is not None:
        return NodeFrame(data, id=id)
    raise ValueError("provide either src= and dst= (EdgeFrame) or id= (NodeFrame)")


def _to_arrow_table(data: Any) -> Any:
    """Normalize any supported in-memory input to a single ``pyarrow.Table``.

    This is the one funnel every in-memory constructor passes through, so once data
    is inside a frame nothing downstream can tell which flavour it came from — the
    "same from here on" guarantee. Detection is dependency-safe: polars / pandas are
    recognized only if already imported (via ``sys.modules``), so neither becomes an
    import-time requirement of Ursa.

    Supported: ``pyarrow.Table`` (as-is), ``pyarrow.RecordBatch``, a list of row
    dicts, a dict of equal-length columns, a ``polars.DataFrame``, a
    ``pandas.DataFrame``, or any object exposing the Arrow C-stream capsule.
    """
    import sys

    import pyarrow as pa

    if isinstance(data, pa.Table):
        return data
    if isinstance(data, pa.RecordBatch):
        return pa.Table.from_batches([data])
    if isinstance(data, list):
        if data and not isinstance(data[0], dict):
            raise TypeError(
                "list input must be a list of row dicts (e.g. "
                "[{'src': 0, 'dst': 1}, ...]); bare tuples/rows are not supported."
            )
        return pa.Table.from_pylist(data)
    if isinstance(data, dict):
        return pa.table(data)
    pl = sys.modules.get("polars")
    if pl is not None and isinstance(data, pl.DataFrame):
        return data.to_arrow()
    pd = sys.modules.get("pandas")
    if pd is not None and isinstance(data, pd.DataFrame):
        return pa.Table.from_pandas(data, preserve_index=False)
    if hasattr(data, "__arrow_c_stream__"):
        return pa.table(data)
    raise TypeError(
        f"cannot build a frame from {type(data).__name__!r}; supported inputs are a "
        "list of row dicts, a dict of columns, a polars.DataFrame, a pandas.DataFrame, "
        "or a pyarrow.Table/RecordBatch."
    )


def _canonical_id_array(arr: Any) -> Any:
    """Canonicalize a node-id array to a supported id type: any integer type to
    int64 (the fast path), string ids passed through unchanged (covering
    UUID-as-string). Raises ``TypeError`` for anything else — Ursa node ids are
    int64 or string.

    ``Utf8`` and ``LargeUtf8`` are both handled natively by the Rust id map (via
    its ``StrView``), so neither is cast here. A ``LargeUtf8 -> Utf8`` cast in
    particular would *panic* rather than error once the string data exceeds the
    2 GiB that ``Utf8``'s i32 offsets can address — the exact input a large_string
    column exists to carry — so the cast is removed, not just skipped."""
    import pyarrow as pa
    import pyarrow.types as pat

    if pat.is_integer(arr.type):
        return arr.cast(pa.int64())
    if pat.is_string(arr.type) or pat.is_large_string(arr.type):
        return arr
    raise TypeError(f"node ids must be an integer or string column; got {arr.type}")


def _node_attr_batch(tbl: Any, id: str) -> Any | None:
    """The node attribute table (an already-normalized ``pyarrow.Table``) as a
    single pyarrow RecordBatch, id canonicalized to int64 or string.

    An unsupported id type or a missing ``id`` column raises immediately at
    construction, so a typo'd column name is reported where the user made it.
    """
    import pyarrow as pa

    idx = tbl.schema.get_field_index(id)
    id_col = tbl.column(id)
    chunks = id_col.chunks if id_col.num_chunks else [id_col.combine_chunks()]
    id_arr = _canonical_id_array(pa.concat_arrays(chunks))
    tbl = tbl.set_column(idx, id, id_arr)
    batches = tbl.combine_chunks().to_batches()
    return batches[0] if batches else None


def _extract_edge_arrays(tbl: Any, src: str, dst: str) -> tuple[Any, Any] | None:
    """Pull the src/dst columns out of an already-normalized ``pyarrow.Table`` as
    contiguous arrays, canonicalized to a supported node-id type (int64 or string).

    An unsupported id type or a missing ``src``/``dst`` column raises immediately at
    construction, so a typo'd column name is reported where the user made it.
    """
    import pyarrow as pa

    arrays = []
    for name in (src, dst):
        column = tbl.column(name)
        chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
        arrays.append(_canonical_id_array(pa.concat_arrays(chunks)))
    return (arrays[0], arrays[1])
