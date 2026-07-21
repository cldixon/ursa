"""Ingress: scan/read/from constructors.

``scan_*`` is lazy (returns a frame that is a plan); ``read_*`` is the eager
convenience (scan + collect). ``from_polars`` / ``from_arrow`` are zero-copy via
Arrow. Object storage is first-class: ``s3://``, ``gs://``, ``az://`` and globs
are accepted, and scans push projections/predicates into Parquet.

Skeleton: constructors build correctly-typed lazy frames with the role mapping
recorded (so ``.src_col`` etc. and ``.explain()`` work now); the actual read is
performed by the engine at ``collect()``.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, overload

from ._frames import EdgeFrame, NodeFrame, _PlanStep

if TYPE_CHECKING:
    from ._result import MaterializedFrame


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
    return EdgeFrame(
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
    return NodeFrame(
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

    Pass ``src``/``dst`` for an EdgeFrame or ``id`` for a NodeFrame.
    """
    return _from_inmemory("from_polars", df, src, dst, id)


@overload
def from_arrow(tbl: Any, *, src: str, dst: str) -> EdgeFrame: ...
@overload
def from_arrow(tbl: Any, *, id: str) -> NodeFrame: ...
def from_arrow(
    tbl: Any, *, src: str | None = None, dst: str | None = None, id: str | None = None
) -> EdgeFrame | NodeFrame:
    """Build a frame from a ``pyarrow.Table``, zero-copy."""
    return _from_inmemory("from_arrow", tbl, src, dst, id)


def _from_inmemory(op: str, data: Any, src, dst, id):
    if src is not None and dst is not None:
        source = _extract_edge_arrays(op, data, src, dst)
        # Retain the full edge table (all columns, same row order as src/dst) so a
        # weight= expression over edge columns can be evaluated later.
        edge_table = None
        try:
            edge_table = data.to_arrow() if op == "from_polars" else data
        except Exception:
            edge_table = None
        return EdgeFrame(
            src_col=src,
            dst_col=dst,
            plan=(_PlanStep(op, {"src": src, "dst": dst}),),
            source=source,
            edge_table=edge_table,
        )
    if id is not None:
        return NodeFrame(
            id_col=id,
            plan=(_PlanStep(op, {"id": id}),),
            source=_node_attr_batch(op, data, id),
        )
    raise ValueError("provide either src= and dst= (EdgeFrame) or id= (NodeFrame)")


def _canonical_id_array(arr: Any) -> Any:
    """Canonicalize a node-id array to a supported id type: any integer type to
    int64 (the fast path), any string type to string (covering UUID-as-string).
    Raises ``TypeError`` for anything else — Ursa node ids are int64 or string."""
    import pyarrow as pa
    import pyarrow.types as pat

    if pat.is_integer(arr.type):
        return arr.cast(pa.int64())
    if pat.is_string(arr.type) or pat.is_large_string(arr.type):
        return arr.cast(pa.string())
    raise TypeError(f"node ids must be an integer or string column; got {arr.type}")


def _node_attr_batch(op: str, data: Any, id: str) -> Any | None:
    """The node attribute table as a single pyarrow RecordBatch (id canonicalized
    to int64 or string).

    Returns None only if pyarrow isn't importable (a source checkout without the
    dependency installed) — ``collect()`` then surfaces a clear error at execution
    time. An unsupported id type or a missing ``id`` column raises immediately at
    construction, so a typo'd column name is reported where the user made it.
    """
    try:
        import pyarrow as pa
    except ImportError:  # pragma: no cover - pyarrow is a hard runtime dependency
        return None

    tbl = data.to_arrow() if op == "from_polars" else data
    idx = tbl.schema.get_field_index(id)
    id_col = tbl.column(id)
    chunks = id_col.chunks if id_col.num_chunks else [id_col.combine_chunks()]
    id_arr = _canonical_id_array(pa.concat_arrays(chunks))
    tbl = tbl.set_column(idx, id, id_arr)
    batches = tbl.combine_chunks().to_batches()
    return batches[0] if batches else None


def _extract_edge_arrays(op: str, data: Any, src: str, dst: str) -> tuple[Any, Any] | None:
    """Pull the src/dst columns out as contiguous pyarrow arrays, canonicalized to
    a supported node-id type (int64 or string).

    Returns None only if pyarrow isn't importable (a source checkout without the
    dependency installed) — `collect()` then surfaces a clear error at execution
    time. An unsupported id type or a missing ``src``/``dst`` column raises
    immediately at construction, so a typo'd column name is reported where the
    user made it.
    """
    try:
        import pyarrow as pa
    except ImportError:  # pragma: no cover - pyarrow is a hard runtime dependency
        return None

    tbl = data.to_arrow() if op == "from_polars" else data
    arrays = []
    for name in (src, dst):
        column = tbl.column(name)
        chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
        arrays.append(_canonical_id_array(pa.concat_arrays(chunks)))
    return (arrays[0], arrays[1])
