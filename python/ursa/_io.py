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
``az://`` and globs are accepted, as is a plain ``http(s)://`` URL for a single
hosted Parquet/CSV file (no globbing over HTTP). Edge scans push their column
projection (``src``/``dst`` plus any weight columns) into Parquet; node-column
projection and predicate pushdown are not yet wired.

However data arrives — scan/read from Parquet, the in-memory constructors, or the
aliases — once it is inside an EdgeFrame/NodeFrame nothing downstream can tell how
it was built; execution is identical from that point on.

Constructors build correctly-typed lazy frames with the role mapping recorded (so
``.src_col`` etc. and ``.explain()`` work eagerly); the file read runs in the
engine at ``collect()``.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, overload

from ._frames import EdgeFrame, NodeFrame, _native

if TYPE_CHECKING:
    from ._result import MaterializedFrame


# A CSV scan's options, with their defaults: how every CSV scan reads.
_CSV_DEFAULTS: dict[str, Any] = {
    "delimiter": ",",
    "has_header": True,
    "quote": '"',
    "escape": None,
    "null_value": None,
    "comment": None,
}


def _csv_options(step: str, path: Any, given: dict[str, Any]) -> dict[str, Any] | None:
    """Check a scan's CSV options and return the ones that differ from the
    defaults, as the engine takes them (a character as its byte), or ``None``
    when all are the defaults, so a default scan builds exactly as before.

    Each character option is one ASCII character other than a line break, and
    no two of ``delimiter``, ``quote``, ``escape`` and ``comment`` may be the
    same character. The options only apply to a CSV file, so passing one to a
    scan with no ``.csv`` path is an error rather than a silent no-op."""
    changed = [k for k, v in given.items() if not _is_default(v, _CSV_DEFAULTS[k])]
    if not changed:
        return None
    has_header = given["has_header"]
    if not isinstance(has_header, bool):
        raise TypeError(f"{step}(): has_header must be True or False; got {has_header!r}.")
    null_value = given["null_value"]
    if null_value is not None and not isinstance(null_value, str):
        raise TypeError(
            f"{step}(): null_value must be a string or None; got {null_value!r}. "
            "Pass the text a null field holds, such as 'NA'."
        )
    chars: dict[str, str] = {}
    for name in ("delimiter", "quote", "escape", "comment"):
        value = given[name]
        if value is None and name in ("escape", "comment"):
            continue
        if not isinstance(value, str) or len(value) != 1 or not value.isascii():
            raise ValueError(
                f"{step}(): {name} must be one ASCII character; got {value!r}. "
                f"Pass a single character, such as {_CSV_EXAMPLE[name]!r}."
            )
        if value in "\r\n":
            raise ValueError(
                f"{step}(): {name} cannot be a line break; got {value!r}. "
                "Rows end at a line break, so pick another character."
            )
        for other, seen in chars.items():
            if seen == value:
                raise ValueError(
                    f"{step}(): {other} and {name} are both {value!r}; "
                    "they must be different characters."
                )
        chars[name] = value
    paths = [path] if isinstance(path, str) else list(path) if isinstance(path, list) else []
    if paths and not any(isinstance(p, str) and p.lower().endswith(".csv") for p in paths):
        names = ", ".join(f"{k}=" for k in changed)
        raise ValueError(
            f"{step}(): the CSV options ({names}) only apply to .csv files, and "
            f"{path!r} is not one. Remove them, or scan a .csv file."
        )
    out: dict[str, Any] = {}
    for name in changed:
        value = given[name]
        out[name] = ord(value) if name in chars else value
    return out


def _is_default(value: Any, default: Any) -> bool:
    """Whether an option was left at its default (``1`` is not ``True``)."""
    return type(value) is type(default) and value == default


# An example value for each character option, for its error message.
_CSV_EXAMPLE = {"delimiter": ";", "quote": "'", "escape": "\\", "comment": "#"}


def _check_paths(step: str, path: Any) -> None:
    """A list of paths needs at least one path."""
    if isinstance(path, list) and not path:
        raise ValueError(f"{step}(): the list of paths is empty. Pass at least one path.")


def scan_edges(
    path: str | list[str],
    *,
    src: str,
    dst: str,
    storage_options: dict[str, Any] | None = None,
    store: Any | None = None,
    on_null: str = "error",
    delimiter: str = ",",
    has_header: bool = True,
    quote: str = '"',
    escape: str | None = None,
    null_value: str | None = None,
    comment: str | None = None,
) -> EdgeFrame:
    """Lazily scan an edge list from Parquet/CSV.

    ``path`` may be local, object storage (``s3://`` / ``gs://`` / ``az://``, globs
    allowed), or a plain ``http(s)://`` URL for a single hosted file. ``src`` /
    ``dst`` are role mappings, not renames. ``store`` accepts a pre-configured
    ``obstore`` store in place of ``storage_options`` (both bind the same
    underlying Rust ``object_store`` crate).

    ``on_null`` controls what happens to a row whose ``src`` or ``dst`` endpoint is
    null: ``"error"`` (default) raises, ``"drop"`` filters the row out (emitting a
    count) instead of failing the whole scan.

    A list of paths reads every path (each a file or a glob) as one table, in
    the order given, as if the files were read one after another and
    concatenated. Each must have the same columns, with the same types.

    The CSV options set how a ``.csv`` file parses: ``delimiter`` (one
    character, ``","`` by default), ``has_header`` (``False`` names the columns
    ``column_1``, ``column_2``, ...), ``quote`` (``'"'``), ``escape`` (a
    character that makes the next one literal; none by default), ``null_value``
    (a text that reads as null, as an empty field always does) and
    ``comment`` (lines starting with it are skipped; none by default).

    HTTP note: a URL must point directly at the file (no query string — the scan
    drops it before fetching, so presigned/token URLs will not authenticate; use
    the ``s3://`` / ``gs://`` / ``az://`` backends with ``storage_options`` for
    signed access). Over plain ``http://`` the request and data are unencrypted;
    treat the URL as untrusted input if it is ever caller-controlled.
    """
    _check_paths("scan_edges", path)
    csv = _csv_options(
        "scan_edges",
        path,
        {
            "delimiter": delimiter,
            "has_header": has_header,
            "quote": quote,
            "escape": escape,
            "null_value": null_value,
            "comment": comment,
        },
    )
    _validate_on_null(on_null)
    spec = {"storage_options": storage_options, "store": store}
    return EdgeFrame._wrap(_native().PlanNode.scan_edges(path, src, dst, on_null, spec, csv))


def scan_nodes(
    path: str | list[str],
    *,
    id: str,
    storage_options: dict[str, Any] | None = None,
    store: Any | None = None,
    delimiter: str = ",",
    has_header: bool = True,
    quote: str = '"',
    escape: str | None = None,
    null_value: str | None = None,
    comment: str | None = None,
) -> NodeFrame:
    """Lazily scan a node/attribute table (local, object storage, or an
    ``http(s)://`` URL); ``id`` is the id-role mapping. A list of paths and the
    CSV options read as in ``scan_edges``."""
    _check_paths("scan_nodes", path)
    csv = _csv_options(
        "scan_nodes",
        path,
        {
            "delimiter": delimiter,
            "has_header": has_header,
            "quote": quote,
            "escape": escape,
            "null_value": null_value,
            "comment": comment,
        },
    )
    spec = {"storage_options": storage_options, "store": store}
    return NodeFrame._wrap(_native().PlanNode.scan_nodes(path, id, spec, csv))


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
def from_arrow(tbl: Any, *, src: str, dst: str, on_null: str = ...) -> EdgeFrame: ...
@overload
def from_arrow(tbl: Any, *, id: str) -> NodeFrame: ...
def from_arrow(
    tbl: Any,
    *,
    src: str | None = None,
    dst: str | None = None,
    id: str | None = None,
    on_null: str = "error",
) -> EdgeFrame | NodeFrame:
    """Build a frame from a ``pyarrow.Table`` / ``RecordBatch``, zero-copy.

    A thin, typed alias for ``EdgeFrame(tbl, src=, dst=)`` / ``NodeFrame(tbl, id=)``.
    ``on_null`` (EdgeFrame only) is ``"error"`` (default) or ``"drop"`` — see
    ``EdgeFrame``.
    """
    return _from_inmemory(tbl, src, dst, id, on_null)


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


def from_edgelist(edges: Any, *, weighted: bool | None = None, on_null: str = "error") -> EdgeFrame:
    """Build an EdgeFrame from an iterable of edge tuples — the most direct way to
    hand-write a small graph.

    Each edge is a 2-tuple ``(src, dst)`` or, for a weighted graph, a 3-tuple
    ``(src, dst, weight)``. Endpoints may be ints or strings (Ursa supports both id
    kinds); the third element, if present, becomes a ``weight`` column. By default
    the arity is auto-detected from the first edge and every edge must match it;
    pass ``weighted=True``/``False`` to require 3-/2-tuples explicitly.

    Bare tuples carry no column names, so they cannot flow through the generic
    in-memory constructor (which needs ``src=``/``dst=`` to name endpoints) — this
    is their sanctioned entry point. The resulting frame is indistinguishable
    downstream from one built any other way.
    """
    import pyarrow as pa

    rows = list(edges)
    if not rows:
        raise ValueError("from_edgelist() needs at least one edge.")
    first = rows[0]
    try:
        arity = len(first)
    except TypeError:
        raise TypeError(
            "each edge must be a 2-tuple (src, dst) or 3-tuple (src, dst, weight); "
            f"got {type(first).__name__!r}."
        ) from None
    if weighted is True:
        arity = 3
    elif weighted is False:
        arity = 2
    if arity not in (2, 3):
        raise ValueError(
            "each edge must be a 2-tuple (src, dst) or 3-tuple (src, dst, weight); "
            f"got a {arity}-tuple."
        )
    srcs, dsts, weights = [], [], []
    for e in rows:
        if len(e) != arity:
            raise ValueError(
                "every edge must have the same arity; expected "
                f"{arity}-tuples but got a {len(e)}-tuple ({e!r}). Pass weighted= to "
                "fix the shape explicitly."
            )
        srcs.append(e[0])
        dsts.append(e[1])
        if arity == 3:
            weights.append(e[2])
    data = {"src": pa.array(srcs), "dst": pa.array(dsts)}
    if arity == 3:
        data["weight"] = pa.array(weights, type=pa.float64())
    return EdgeFrame(pa.table(data), src="src", dst="dst", on_null=on_null)


def _from_inmemory(data: Any, src, dst, id, on_null: str = "error"):
    """Dispatch an in-memory input to the right primary constructor. The public
    ``from_arrow`` / ``from_polars`` / ``from_pandas`` aliases all funnel here; the
    constructors normalize any supported flavour to one canonical Arrow source.
    ``on_null`` applies only to the EdgeFrame path (endpoint null policy)."""
    if src is not None and dst is not None:
        return EdgeFrame(data, src=src, dst=dst, on_null=on_null)
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

    ``string_view`` columns (what DataFusion, Athena and polars' Arrow C stream
    hand back) become plain strings, as a scan reads them: see
    ``_without_string_views``.
    """
    return _without_string_views(_as_arrow_table(data))


def _as_arrow_table(data: Any) -> Any:
    """The flavour dispatch behind ``_to_arrow_table``."""
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


# The most character data an ``Utf8`` (i32-offset) array can address.
_UTF8_MAX_BYTES = 2**31 - 1


def _without_string_views(tbl: Any) -> Any:
    """``tbl`` with each top-level ``string_view`` column cast to ``string``.

    The Rust side reads string views natively, but the relational tail filters and
    sorts with pyarrow compute, which has few string-view kernels (no ``take``, no
    comparisons), so a view column there fails on the first ``filter``/``sort``.
    Converting once at ingress keeps every frame on the types the whole pipeline
    supports, and matches what a scan returns. A column whose chunks may hold more
    than 2 GiB of character data goes to ``large_string`` instead, since a cast to
    ``string`` past ``Utf8``'s i32 offsets fails (#152). Other columns are shared,
    not copied."""
    import pyarrow as pa
    import pyarrow.types as pat

    for i, field in enumerate(tbl.schema):
        if not pat.is_string_view(field.type):
            continue
        column = tbl.column(i)
        # ``nbytes`` (views plus data buffers) bounds the character data from above.
        fits = all(chunk.nbytes <= _UTF8_MAX_BYTES for chunk in column.chunks)
        target = pa.string() if fits else pa.large_string()
        tbl = tbl.set_column(i, field.with_type(target), column.cast(target))
    return tbl


def _canonical_id_array(arr: Any) -> Any:
    """Canonicalize a node-id array to a supported id type: any integer type to
    int64 (the fast path), string ids passed through unchanged (covering
    UUID-as-string). Raises ``TypeError`` for anything else — Ursa node ids are
    int64 or string.

    ``Utf8``, ``LargeUtf8`` and ``Utf8View`` (``string_view``, what DataFusion,
    Athena and Polars hand back for strings) are all handled natively by the Rust
    id map (via its ``StrView``), so none is cast here. A ``LargeUtf8 -> Utf8``
    cast in particular would *panic* rather than error once the string data
    exceeds the 2 GiB that ``Utf8``'s i32 offsets can address — the exact input a
    large_string column exists to carry — so the cast is removed, not just
    skipped. A dictionary-encoded column (a categorical) is decoded and then
    canonicalized by its value type."""
    import pyarrow as pa
    import pyarrow.types as pat

    t = arr.type
    if pat.is_integer(t):
        return arr.cast(pa.int64())
    if _is_any_string(t):
        return arr
    if pat.is_dictionary(t) and (pat.is_integer(t.value_type) or _is_any_string(t.value_type)):
        return _canonical_id_array(arr.dictionary_decode())
    raise TypeError(f"node ids must be an integer or string column; got {t}")


def _is_any_string(t: Any) -> bool:
    """Whether an Arrow type is one of the string layouts (``string``,
    ``large_string``, ``string_view``)."""
    import pyarrow.types as pat

    return pat.is_string(t) or pat.is_large_string(t) or pat.is_string_view(t)


def _node_attr_table(tbl: Any, id: str) -> Any:
    """The node attribute table (an already-normalized ``pyarrow.Table``) with its
    id column canonicalized to int64 or string, chunk by chunk.

    The table's chunks are kept as they are (no ``combine_chunks``), so attribute
    columns are shared with the caller's table rather than copied. A 0-row table
    stays a valid, empty table, so an empty node set is still a materializable
    frame.

    An unsupported id type or a missing ``id`` column raises immediately at
    construction, so a typo'd column name is reported where the user made it.
    """
    import pyarrow as pa

    idx = tbl.schema.get_field_index(id)
    id_col = tbl.column(id)
    chunks = id_col.chunks if id_col.num_chunks else [id_col.combine_chunks()]
    canonical = [_canonical_id_array(c) for c in chunks]
    return tbl.set_column(idx, id, pa.chunked_array(canonical, type=canonical[0].type))


def _validate_on_null(on_null: str) -> None:
    """``on_null`` accepts only ``"error"`` (the safe default — a null endpoint
    raises) or ``"drop"`` (rows with a null src/dst are filtered out); the engine
    holds the list."""
    _native().check_on_null(on_null)


def _warn_dropped_endpoints(n: int) -> None:
    """Announce how many edge rows ``on_null="drop"`` removed, so a silent drop can
    never masquerade as 'all rows ingested' (the honesty policy, #55)."""
    if n:
        import warnings

        warnings.warn(
            f"on_null='drop': dropped {n} edge row(s) with a null src or dst endpoint.",
            stacklevel=2,
        )


def _drop_null_endpoint_rows(tbl: Any, src: str, dst: str) -> tuple[Any, int]:
    """Return ``(filtered_table, n_dropped)`` — rows whose ``src`` or ``dst``
    endpoint is null are removed. The in-memory half of the ``on_null="drop"``
    opt-in; filtering the whole table here keeps the endpoint arrays and the
    retained edge table (for weights) row-aligned automatically."""
    import pyarrow.compute as pc

    # pyarrow.compute members are looked up by name (the type checker lacks its stubs).
    is_valid, and_ = pc.is_valid, pc.and_  # ty: ignore[unresolved-attribute]
    mask = and_(is_valid(tbl.column(src)), is_valid(tbl.column(dst)))
    kept = tbl.filter(mask)
    return kept, tbl.num_rows - kept.num_rows


def _extract_edge_arrays(tbl: Any, src: str, dst: str) -> tuple[Any, Any] | None:
    """Pull the src/dst columns out of an already-normalized ``pyarrow.Table`` as
    chunked arrays, canonicalized to a supported node-id type (int64 or string).

    The chunks are kept as they are, not concatenated: an int64 or string column
    is then shared with the retained edge table rather than copied, which would
    double the endpoint memory for the life of the frame. ``_require_edges``
    slices the build batches out of them.

    An unsupported id type or a missing ``src``/``dst`` column raises immediately at
    construction, so a typo'd column name is reported where the user made it.
    """
    import pyarrow as pa

    arrays = []
    for name in (src, dst):
        column = tbl.column(name)
        chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
        canonical = [_canonical_id_array(c) for c in chunks]
        arrays.append(pa.chunked_array(canonical, type=canonical[0].type))
    return (arrays[0], arrays[1])
