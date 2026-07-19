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

from typing import Any

from ._frames import EdgeFrame, NodeFrame, _PlanStep


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
        {"path": path, "src": src, "dst": dst,
         "storage_options": storage_options, "store": store, "format_opts": format_opts},
    )
    return EdgeFrame(src_col=src, dst_col=dst, plan=(step,))


def scan_nodes(
    path: str | list[str],
    *,
    id: str,  # noqa: A002  (mirrors the spec's keyword)
    storage_options: dict[str, Any] | None = None,
    store: Any | None = None,
    **format_opts: Any,
) -> NodeFrame:
    """Lazily scan a node/attribute table; ``id`` is the id-role mapping."""
    step = _PlanStep(
        "scan_nodes",
        {"path": path, "id": id,
         "storage_options": storage_options, "store": store, "format_opts": format_opts},
    )
    return NodeFrame(id_col=id, plan=(step,))


def read_edges(path: str | list[str], **kwargs: Any) -> EdgeFrame:
    """Eager convenience: ``scan_edges(...).collect()``."""
    return scan_edges(path, **kwargs).collect()  # type: ignore[return-value]


def read_nodes(path: str | list[str], **kwargs: Any) -> NodeFrame:
    """Eager convenience: ``scan_nodes(...).collect()``."""
    return scan_nodes(path, **kwargs).collect()  # type: ignore[return-value]


def from_polars(df: Any, *, src: str | None = None, dst: str | None = None,
                id: str | None = None) -> EdgeFrame | NodeFrame:  # noqa: A002
    """Build a frame from an in-memory ``polars.DataFrame``, zero-copy via Arrow.

    Pass ``src``/``dst`` for an EdgeFrame or ``id`` for a NodeFrame.
    """
    return _from_inmemory("from_polars", df, src, dst, id)


def from_arrow(tbl: Any, *, src: str | None = None, dst: str | None = None,
               id: str | None = None) -> EdgeFrame | NodeFrame:  # noqa: A002
    """Build a frame from a ``pyarrow.Table``, zero-copy."""
    return _from_inmemory("from_arrow", tbl, src, dst, id)


def _from_inmemory(op: str, data: Any, src, dst, id):  # noqa: A002
    if src is not None and dst is not None:
        return EdgeFrame(src_col=src, dst_col=dst, plan=(_PlanStep(op, {"src": src, "dst": dst}),))
    if id is not None:
        return NodeFrame(id_col=id, plan=(_PlanStep(op, {"id": id}),))
    raise ValueError("provide either src= and dst= (EdgeFrame) or id= (NodeFrame)")
