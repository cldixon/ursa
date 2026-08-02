"""Interop constructors for the scientific-Python graph ecosystem.

These build Ursa frames from the data structures users already have — a
``networkx`` graph, a ``numpy`` adjacency/edge array, or a ``scipy.sparse``
matrix. Each optional library is imported **inside** the function that needs it,
never at module load, so ``import ursa`` never drags in networkx/numpy/scipy;
a caller of ``from_networkx`` plainly already has networkx, and if not the error
says exactly what to install.

Every constructor funnels to ``EdgeFrame(table, src="src", dst="dst")`` (or a
``NodeFrame`` for node attributes), so a frame built here is indistinguishable
downstream from one built any other way — the "same from here on" guarantee.
"""

from __future__ import annotations

from typing import Any

from ._frames import EdgeFrame, NodeFrame


def _require(module: str, pip: str | None = None) -> Any:
    """Import an optional interop dependency, or raise a clear, actionable error
    naming what to install. (The library is a hard requirement of *this*
    constructor, not of Ursa — so a guarded import here, not a ``sys.modules``
    probe.)"""
    try:
        return __import__(module)
    except ImportError as exc:  # pragma: no cover - exercised via monkeypatch
        pkg = pip or module
        raise ImportError(
            f"this constructor requires {module!r}; install it with `pip install {pkg}` "
            f"(or `pip install ursa-graph[{module}]`)."
        ) from exc


# --- networkx --------------------------------------------------------------
def from_networkx(graph: Any, *, weight: str = "weight") -> EdgeFrame:
    """Build an EdgeFrame from a ``networkx`` graph.

    Each graph edge becomes one Ursa edge row (``src``/``dst`` from the endpoints).
    If **every** edge carries the ``weight`` attribute key, a ``weight`` column is
    added; otherwise the frame is unweighted. Node ids are the networkx node
    labels (int or string — Ursa supports both).

    An undirected ``networkx.Graph`` yields each edge once; a ``DiGraph`` yields
    directed rows. Ursa treats directedness as a per-operation parameter, so
    either is fine — symmetrize beforehand if you want undirected traversals over
    a ``Graph``. Node **attributes** are not pulled in here; use
    :func:`nodes_from_networkx` for the attribute table.
    """
    import pyarrow as pa

    _require("networkx")
    srcs: list[Any] = []
    dsts: list[Any] = []
    weights: list[Any] = []
    all_weighted = True
    for u, v, attrs in graph.edges(data=True):
        srcs.append(u)
        dsts.append(v)
        if weight in attrs:
            weights.append(attrs[weight])
        else:
            all_weighted = False
    data = {"src": pa.array(srcs), "dst": pa.array(dsts)}
    if all_weighted and srcs:
        data["weight"] = pa.array(weights, type=pa.float64())
    return EdgeFrame(pa.table(data), src="src", dst="dst")


def nodes_from_networkx(graph: Any, *, id: str = "id") -> NodeFrame:
    """Build a NodeFrame of node attributes from a ``networkx`` graph.

    Produces one row per node with the id column named ``id`` (override via
    ``id=``) plus a column for every attribute key that appears on any node
    (missing values are null). Node ids are the networkx node labels.
    """
    import pyarrow as pa

    _require("networkx")
    keys = _all_node_attr_keys(graph)
    if id in keys:
        raise ValueError(
            f"id column name {id!r} collides with a node attribute of the same name; "
            "pass a different id= name."
        )
    ids: list[Any] = []
    columns: dict[str, list[Any]] = {k: [] for k in keys}
    for node, attrs in graph.nodes(data=True):
        ids.append(node)
        for k in keys:
            columns[k].append(attrs.get(k))
    data: dict[str, Any] = {id: pa.array(ids)}
    for k in keys:
        data[k] = pa.array(columns[k])
    return NodeFrame(pa.table(data), id=id)


def _all_node_attr_keys(graph: Any) -> list[str]:
    """Every attribute key present on any node, in first-seen order (stable output
    schema regardless of dict iteration quirks)."""
    seen: dict[str, None] = {}
    for _node, attrs in graph.nodes(data=True):
        for k in attrs:
            seen.setdefault(k, None)
    return list(seen)


# --- numpy -----------------------------------------------------------------
def from_numpy(array: Any, *, kind: str = "auto", weighted: bool = False) -> EdgeFrame:
    """Build an EdgeFrame from a ``numpy`` array.

    Two shapes are supported, selected by ``kind``:

    - ``"adjacency"`` — a square ``(N, N)`` matrix; every nonzero entry ``[i, j]``
      is an edge ``i -> j``. With ``weighted=True`` the entry value becomes the
      ``weight`` column.
    - ``"edges"`` — an ``(M, 2)`` array of ``[src, dst]`` rows, or ``(M, 3)`` of
      ``[src, dst, weight]`` (a third column always carries the weight).

    ``kind="auto"`` (the default) picks ``"adjacency"`` for a square 2-D array and
    ``"edges"`` for an ``(M, 2)`` / ``(M, 3)`` one; an ambiguous shape (e.g.
    ``(2, 2)``, which is both) resolves to ``"adjacency"`` — pass ``kind`` to be
    explicit.
    """
    import pyarrow as pa

    np = _require("numpy")
    arr = np.asarray(array)
    if arr.ndim != 2:
        raise ValueError(f"from_numpy() expects a 2-D array; got {arr.ndim}-D.")
    resolved = kind
    if kind == "auto":
        if arr.shape[0] == arr.shape[1]:
            resolved = "adjacency"
        elif arr.shape[1] in (2, 3):
            resolved = "edges"
        else:
            raise ValueError(
                f"cannot infer kind from shape {arr.shape}; pass kind='adjacency' or kind='edges'."
            )
    if resolved == "adjacency":
        if arr.shape[0] != arr.shape[1]:
            raise ValueError(f"adjacency matrix must be square; got shape {arr.shape}.")
        rows, cols = np.nonzero(arr)
        data = {"src": rows.astype("int64"), "dst": cols.astype("int64")}
        if weighted:
            data["weight"] = arr[rows, cols].astype("float64")
        return EdgeFrame(pa.table(data), src="src", dst="dst")
    if resolved == "edges":
        if arr.shape[1] not in (2, 3):
            raise ValueError(
                f"edge array must have 2 or 3 columns (src, dst[, weight]); got shape {arr.shape}."
            )
        data = {"src": arr[:, 0].astype("int64"), "dst": arr[:, 1].astype("int64")}
        if arr.shape[1] == 3:
            data["weight"] = arr[:, 2].astype("float64")
        return EdgeFrame(pa.table(data), src="src", dst="dst")
    raise ValueError(f"kind must be 'auto', 'adjacency', or 'edges'; got {kind!r}.")


# --- scipy.sparse ----------------------------------------------------------
def from_scipy_sparse(matrix: Any, *, weighted: bool = False) -> EdgeFrame:
    """Build an EdgeFrame from a ``scipy.sparse`` adjacency matrix.

    Any sparse format (COO/CSR/CSC/…) is accepted; it is converted to COO and each
    stored entry ``[i, j]`` becomes an edge ``i -> j``. With ``weighted=True`` the
    stored value becomes the ``weight`` column. Explicitly-stored zeros are kept as
    edges (they are part of the sparsity pattern the caller chose to store).
    """
    import pyarrow as pa

    _require("scipy", pip="scipy")
    if not hasattr(matrix, "tocoo"):
        raise TypeError(
            f"from_scipy_sparse() expects a scipy.sparse matrix; got {type(matrix).__name__!r}."
        )
    coo = matrix.tocoo()
    data = {"src": coo.row.astype("int64"), "dst": coo.col.astype("int64")}
    if weighted:
        data["weight"] = coo.data.astype("float64")
    return EdgeFrame(pa.table(data), src="src", dst="dst")
