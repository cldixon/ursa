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
        scan={"path": path, "src": src, "dst": dst},
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
    return NodeFrame(id_col=id, plan=(step,), scan={"path": path, "id": id})


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
        return EdgeFrame(
            src_col=src,
            dst_col=dst,
            plan=(_PlanStep(op, {"src": src, "dst": dst}),),
            source=source,
        )
    if id is not None:
        return NodeFrame(
            id_col=id,
            plan=(_PlanStep(op, {"id": id}),),
            source=_node_attr_batch(op, data, id),
        )
    raise ValueError("provide either src= and dst= (EdgeFrame) or id= (NodeFrame)")


def _node_attr_batch(op: str, data: Any, id: str) -> Any | None:
    """The node attribute table as a single pyarrow RecordBatch (id cast to int64).

    Returns None if pyarrow isn't importable — ``collect()`` then surfaces a clear
    error at execution time rather than at construction.
    """
    try:
        import pyarrow as pa

        tbl = data.to_arrow() if op == "from_polars" else data
        idx = tbl.schema.get_field_index(id)
        id_col = tbl.column(id)
        chunks = id_col.chunks if id_col.num_chunks else [id_col.combine_chunks()]
        id_arr = pa.concat_arrays(chunks).cast(pa.int64())
        tbl = tbl.set_column(idx, id, id_arr)
        batches = tbl.combine_chunks().to_batches()
        return batches[0] if batches else None
    except Exception:
        return None


def _extract_edge_arrays(op: str, data: Any, src: str, dst: str) -> tuple[Any, Any] | None:
    """Pull the src/dst columns out as contiguous int64 pyarrow arrays.

    Returns None (rather than raising) if pyarrow isn't importable or the columns
    can't be resolved — `collect()` then surfaces a clear error at execution time
    instead of at construction. Node ids are cast to int64 (the v0.1 node-id type).
    """
    try:
        import pyarrow as pa

        tbl = data.to_arrow() if op == "from_polars" else data
        arrays = []
        for name in (src, dst):
            column = tbl.column(name)
            chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
            arrays.append(pa.concat_arrays(chunks).cast(pa.int64()))
        return (arrays[0], arrays[1])
    except Exception:
        return None
